#!/usr/bin/env bash
# Seed a status page where one outage takes five components down together,
# the shape that sends every subscriber one mail per component.
#
# Page `outage-demo` carries five manual monitors (nothing probes them):
# Website, API, Dashboard, Checkout, Webhooks. The script then
#
#   history   writes the same outage, ended, yesterday (19:02, about 2 minutes)
#             and one Checkout incident three days ago, so the history shows
#             the fold next to a single row;
#   now       sets all five down through the API, so the incident writer opens
#             five live incidents the way production does, with their
#             generated "investigating" updates.
#
#   RECOVER=1 sets the five back up instead; the incidents close after their
#             recovery hold.
#
# Env overrides:
#   SLUG          org slug to seed onto    (default: devorg)
#   PAGE_SLUG     status page slug         (default: outage-demo)
#   PG_CONTAINER  postgres container name  (default: uptimepage-postgres-1)
#   APP           where the app listens    (default: http://127.0.0.1:8080)
#   BASE_DOMAIN   public status base       (default: lvh.me)
#   TOKEN         dev session cookie       (default: devsession-localtest-0000000000)
#
# Idempotent: the monitors are tagged `seed-og`; they, their incidents and the
# page are wiped before re-insert. Requires `just up`, `just dev-login` and the
# app running in subdomain mode (`just run`).
set -euo pipefail

SLUG="${SLUG:-devorg}"
PAGE_SLUG="${PAGE_SLUG:-outage-demo}"
PG_CONTAINER="${PG_CONTAINER:-uptimepage-postgres-1}"
APP="${APP:-http://127.0.0.1:8080}"
BASE_DOMAIN="${BASE_DOMAIN:-lvh.me}"
TOKEN="${TOKEN:-devsession-localtest-0000000000}"
PORT="${APP##*:}"
HOST="${PAGE_SLUG}.${BASE_DOMAIN}:${PORT}"
OPERATOR="app.${BASE_DOMAIN}:${PORT}"

pg() { docker exec -i "$PG_CONTAINER" psql -U monitor -d monitor -v ON_ERROR_STOP=1 "$@"; }

ORG=$(pg -tAc "SELECT id FROM organizations WHERE slug='${SLUG}' AND deleted_at IS NULL")
if [[ -z "$ORG" ]]; then
  echo "error: org '${SLUG}' missing; run 'just dev-login' first" >&2
  exit 1
fi

api_code=$(curl -s -o /dev/null -w '%{http_code}' -H "Host: ${OPERATOR}" \
  -H "Cookie: _sm_session=${TOKEN}" "${APP}/api/v1/targets" || true)
if [[ "$api_code" != "200" ]]; then
  echo "error: the app at ${APP} answered '${api_code:-nothing}' for ${OPERATOR}." >&2
  echo "       Start it with 'just run' and sign in with 'just dev-login'." >&2
  exit 1
fi

set_state() {
  local status=$1
  local ids
  ids=$(pg -tAc "SELECT id FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['seed-og'] ORDER BY name")
  for id in $ids; do
    curl -s -o /dev/null -w "    ${status} %{http_code}\n" -X PUT \
      -H "Host: ${OPERATOR}" -H "Cookie: _sm_session=${TOKEN}" \
      -H 'X-Requested-With: uptimepage' -H 'Content-Type: application/json' \
      -d "{\"status\":\"${status}\"}" "${APP}/api/v1/targets/${id}/state"
  done
}

if [[ "${RECOVER:-0}" == "1" ]]; then
  echo "==> Setting the five monitors up"
  set_state up
  echo "    The incidents close after their recovery hold; refresh http://${HOST}/"
  exit 0
fi

echo "==> Postgres: (re)create page '${PAGE_SLUG}', its monitors and their history"
pg -q <<SQL
BEGIN;

DELETE FROM incidents
 WHERE org_id = '${ORG}'
   AND target_id IN (SELECT id FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['seed-og']);
DELETE FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['seed-og'];
DELETE FROM status_pages WHERE org_id = '${ORG}' AND slug = '${PAGE_SLUG}';

CREATE TEMP TABLE seed_components (tag text, name text, sort int) ON COMMIT DROP;
INSERT INTO seed_components VALUES
  ('og-website', 'Website', 0), ('og-api', 'API', 1), ('og-dashboard', 'Dashboard', 2),
  ('og-checkout', 'Checkout', 3), ('og-webhooks', 'Webhooks', 4);

INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled, tags, group_name)
SELECT '${ORG}', name || ' (seeded)', '{"type":"manual"}'::jsonb, 60, true,
       ARRAY['seed-og', tag], 'Outage demo'
  FROM seed_components;

INSERT INTO status_pages (org_id, slug, name, enabled)
VALUES ('${ORG}', '${PAGE_SLUG}', 'Outage demo', true);

INSERT INTO status_page_components (org_id, status_page_id, target_id, public_name, sort_order)
SELECT '${ORG}', p.id, t.id, c.name, c.sort
  FROM status_pages p
  JOIN seed_components c ON true
  JOIN targets t ON t.org_id = '${ORG}' AND t.tags @> ARRAY[c.tag]
 WHERE p.org_id = '${ORG}' AND p.slug = '${PAGE_SLUG}';

-- Yesterday's outage: five components, starts 11 s apart, each down 2 minutes.
-- Three days ago: Checkout alone for 15 minutes.
WITH past AS (
  INSERT INTO incidents (org_id, target_id, started_at, ended_at, duration_secs, status_at_start,
                         visibility, state, resolved_at)
  SELECT '${ORG}', t.id, v.started, v.started + v.length, extract(epoch FROM v.length)::int, 'down',
         'public', 'resolved', v.started + v.length + interval '3 minutes'
    FROM (VALUES
            ('og-website',   date_trunc('day', now()) - interval '1 day' + interval '19:02:26', interval '2 minutes'),
            ('og-api',       date_trunc('day', now()) - interval '1 day' + interval '19:02:28', interval '2 minutes'),
            ('og-dashboard', date_trunc('day', now()) - interval '1 day' + interval '19:02:30', interval '2 minutes'),
            ('og-checkout',  date_trunc('day', now()) - interval '1 day' + interval '19:02:30', interval '2 minutes'),
            ('og-webhooks',  date_trunc('day', now()) - interval '1 day' + interval '19:02:37', interval '2 minutes'),
            ('og-checkout',  date_trunc('day', now()) - interval '3 days' + interval '14:10:05', interval '15 minutes'))
         AS v(tag, started, length)
    JOIN targets t ON t.org_id = '${ORG}' AND t.tags @> ARRAY[v.tag]
  RETURNING id, org_id, started_at, ended_at, resolved_at
)
INSERT INTO incident_updates (org_id, incident_id, phase, message, author, generated, posted_at)
SELECT org_id, id, 'investigating', 'Automatically detected — monitoring checks are failing.', 'system', true,
       started_at + interval '80 seconds'
  FROM past
UNION ALL
SELECT org_id, id, 'resolved', 'Automatically resolved — monitoring checks have recovered.', 'system', true,
       resolved_at
  FROM past;

COMMIT;
SQL
echo "    seeded: 5 monitors on page ${PAGE_SLUG}, 6 past incidents"

echo "==> API: setting the five monitors down"
set_state down

echo "==> Waiting for the incident writer to open them (about 30 s)"
active=0
for _ in $(seq 1 30); do
  active=$(curl -s -H "Host: ${HOST}" "${APP}/api/public/v1/status" \
    | jq '.active_incidents | length' 2>/dev/null || echo 0)
  [[ "$active" -ge 5 ]] && break
  sleep 3
done
echo "    active incidents on the page: ${active}"

echo
echo "==> Open in a browser:"
echo "    http://${HOST}/                      status page (active incidents + history)"
echo "    http://${HOST}/status/incidents      archive"
echo "    http://${OPERATOR}/incidents         operator incidents list"
echo
echo "    RECOVER=1 $0   sets them back up"
