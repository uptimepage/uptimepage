#!/usr/bin/env bash
# Seed a status page whose history mixes incidents and maintenance windows,
# then check what the running app serves for it.
#
# Page `history-demo` carries two manual monitors (nothing probes them), and a
# third monitor sits off the page. Windows, relative to now:
#
#   Database upgrade       2 days ago, 1h, short note   → history, in view
#   Network maintenance    12 days ago, 3h, long note   → history, folded, opens
#   Certificate rotation   20 days ago, 30m, no note    → history, folded
#   Storage migration      45 days ago, 2h              → archive only
#   Load balancer swap     running now                  → banner + archive
#   Kernel patching        in 2 days                    → upcoming only
#   Cancelled window       5 days ago, cancelled        → nowhere
#   Back office move       3 days ago, monitor off page → nowhere
#
# plus two public incidents, 1 and 9 days ago, so the All tab mixes both.
#
# Env overrides:
#   SLUG          org slug to seed onto    (default: devorg)
#   PAGE_SLUG     status page slug         (default: history-demo)
#   PG_CONTAINER  postgres container name  (default: uptimepage-postgres-1)
#   APP           where the app listens    (default: http://127.0.0.1:8080)
#   BASE_DOMAIN   public status base       (default: lvh.me)
#
# Idempotent: the monitors are tagged `seed-mh`; they, their windows and
# incidents, and the page are wiped before re-insert.
#
# Requires `just up` + `just dev-login`. The checks run when the app is up in
# subdomain mode (`just run`); without it the seed still lands and the checks
# are skipped.
set -euo pipefail

SLUG="${SLUG:-devorg}"
PAGE_SLUG="${PAGE_SLUG:-history-demo}"
PG_CONTAINER="${PG_CONTAINER:-uptimepage-postgres-1}"
APP="${APP:-http://127.0.0.1:8080}"
BASE_DOMAIN="${BASE_DOMAIN:-lvh.me}"
PORT="${APP##*:}"
HOST="${PAGE_SLUG}.${BASE_DOMAIN}:${PORT}"

pg() { docker exec -i "$PG_CONTAINER" psql -U monitor -d monitor -v ON_ERROR_STOP=1 "$@"; }

ORG=$(pg -tAc "SELECT id FROM organizations WHERE slug='${SLUG}' AND deleted_at IS NULL")
if [[ -z "$ORG" ]]; then
  echo "error: org '${SLUG}' missing — run 'just dev-login' (or SLUG=… just dev-login) first" >&2
  exit 1
fi

echo "==> Postgres: (re)create page '${PAGE_SLUG}', its monitors, windows and incidents"
pg -q <<SQL
BEGIN;

DELETE FROM maintenance_windows
 WHERE org_id = '${ORG}'
   AND id IN (SELECT mwc.maintenance_id FROM maintenance_window_components mwc
               JOIN targets t ON t.id = mwc.target_id
              WHERE t.org_id = '${ORG}' AND t.tags @> ARRAY['seed-mh']);
DELETE FROM incidents
 WHERE org_id = '${ORG}'
   AND target_id IN (SELECT id FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['seed-mh']);
DELETE FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['seed-mh'];
DELETE FROM status_pages WHERE org_id = '${ORG}' AND slug = '${PAGE_SLUG}';

INSERT INTO monitor_groups (org_id, name) VALUES ('${ORG}', 'Seeded') ON CONFLICT (org_id, name) DO NOTHING;
INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled, tags, group_id)
VALUES
  ('${ORG}', 'Website (seeded)',     '{"type":"manual"}'::jsonb, 60, true, ARRAY['seed-mh','mh-website'], (SELECT id FROM monitor_groups WHERE org_id = '${ORG}' AND name = 'Seeded')),
  ('${ORG}', 'API (seeded)',         '{"type":"manual"}'::jsonb, 60, true, ARRAY['seed-mh','mh-api'],     (SELECT id FROM monitor_groups WHERE org_id = '${ORG}' AND name = 'Seeded')),
  ('${ORG}', 'Back office (seeded)', '{"type":"manual"}'::jsonb, 60, true, ARRAY['seed-mh','mh-office'],  (SELECT id FROM monitor_groups WHERE org_id = '${ORG}' AND name = 'Seeded'));

INSERT INTO status_pages (org_id, slug, name, enabled)
VALUES ('${ORG}', '${PAGE_SLUG}', 'History demo', true);

INSERT INTO status_page_components (org_id, status_page_id, target_id, public_name, sort_order)
SELECT '${ORG}', p.id, t.id, v.public_name, v.sort_order
  FROM status_pages p,
       (VALUES ('mh-website', 'Website', 0), ('mh-api', 'API', 1)) AS v(tag, public_name, sort_order)
  JOIN targets t ON t.org_id = '${ORG}' AND t.tags @> ARRAY[v.tag]
 WHERE p.slug = '${PAGE_SLUG}';

CREATE TEMP TABLE seed_windows (title text, note text, starts interval, length interval,
                                tags text[], cancelled boolean) ON COMMIT DROP;
INSERT INTO seed_windows VALUES
  ('Database upgrade', 'Primary database moves to new hardware. Writes pause for a few minutes.',
   interval '-2 days', interval '1 hour', ARRAY['mh-api'], false),
  ('Network maintenance', 'Our upstream provider replaces core switches in the Frankfurt data centre. '
   'Connections may drop for up to a minute while traffic moves to the standby links. We keep the API '
   'and the dashboard reachable throughout, but long-running uploads may need a retry once the window ends.',
   interval '-12 days', interval '3 hours', ARRAY['mh-website','mh-api'], false),
  ('Certificate rotation', NULL, interval '-20 days', interval '30 minutes', ARRAY['mh-website'], false),
  ('Storage migration', 'Object storage moves to a new cluster.',
   interval '-45 days', interval '2 hours', ARRAY['mh-api'], false),
  ('Load balancer swap', 'New load balancers take over, one zone at a time.',
   interval '-30 minutes', interval '2 hours', ARRAY['mh-website'], false),
  ('Kernel patching', 'Rolling reboots, one node at a time.',
   interval '2 days', interval '1 hour', ARRAY['mh-api'], false),
  ('Cancelled window', 'Called off.', interval '-5 days', interval '1 hour', ARRAY['mh-api'], true),
  ('Back office move', 'Internal only.', interval '-3 days', interval '1 hour', ARRAY['mh-office'], false);

WITH w AS (
  INSERT INTO maintenance_windows (org_id, title, description, starts_at, ends_at, deleted_at)
  SELECT '${ORG}', title, note, now() + starts, now() + starts + length,
         CASE WHEN cancelled THEN now() END
    FROM seed_windows
  RETURNING id, title
)
INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id)
SELECT '${ORG}', w.id, t.id
  FROM w
  JOIN seed_windows s ON s.title = w.title
  JOIN targets t ON t.org_id = '${ORG}' AND t.tags && s.tags AND t.tags @> ARRAY['seed-mh'];

INSERT INTO incidents (org_id, target_id, started_at, ended_at, status_at_start, visibility, public_title)
SELECT '${ORG}', t.id, now() + v.starts, now() + v.starts + v.length, 'down', 'public', v.title
  FROM (VALUES ('mh-website', 'Checkout errors (seeded)',     interval '-1 day',  interval '12 minutes'),
               ('mh-api',     'Slow API responses (seeded)',  interval '-9 days', interval '25 minutes'))
       AS v(tag, title, starts, length)
  JOIN targets t ON t.org_id = '${ORG}' AND t.tags @> ARRAY[v.tag];

COMMIT;
SQL
echo "    seeded: 8 windows, 2 incidents, page ${PAGE_SLUG}"

echo
echo "==> Open in a browser (app running via 'just run'):"
echo "    http://${HOST}/"
echo "    http://${HOST}/?show=maintenance#history"
echo "    http://${HOST}/status/incidents?show=maintenance"
echo "    http://${HOST}/api/public/v1/maintenance/past"
echo

code=$(curl -s -o /dev/null -w '%{http_code}' -H "Host: ${HOST}" "${APP}/api/public/v1/status" || true)
if [[ "$code" != "200" ]]; then
  echo "==> Checks skipped: ${APP} answered '${code:-nothing}' for ${HOST}."
  echo "    Start the app with 'just run' (subdomain mode on ${BASE_DOMAIN}) and run this again."
  exit 0
fi

failed=0
pass() { echo "    ok    $1"; }
fail() { echo "    FAIL  $1"; failed=1; }

get() { curl -s -H "Host: ${HOST}" "${APP}$1" || true; }

# expect_titles <path> <jq filter> <expected, comma separated>
expect_titles() {
  local got
  got=$(get "$1" | jq -r "[$2 | .title] | join(\",\")" 2>/dev/null || true)
  if [[ "$got" == "$3" ]]; then pass "$1 $2 = $3"; else fail "$1 $2 = '$got', want '$3'"; fi
}

# expect_in <label> <body> <has|lacks> <text>
expect_in() {
  if [[ "$3" == has ]] && grep -qF -- "$4" <<<"$2"; then pass "$1 has '$4'"
  elif [[ "$3" == lacks ]] && ! grep -qF -- "$4" <<<"$2"; then pass "$1 lacks '$4'"
  else fail "$1 should $3 '$4'"; fi
}

# expect_page <path> <has|lacks> <text>: anywhere on the page.
expect_page() { expect_in "$1" "$(get "$1")" "$2" "$3"; }

# expect_history <path> <has|lacks> <text>: the history section only. The day
# bars' popover data names a day's incidents on every tab, so a whole-page
# check cannot tell which rows a tab lists.
expect_history() {
  expect_in "$1 #history" "$(get "$1" | sed -n '/<section id="history"/,/<\/section>/p')" "$2" "$3"
}

echo "==> JSON API"
expect_titles /api/public/v1/status '.recent_maintenance[]' \
  'Database upgrade,Network maintenance,Certificate rotation'
expect_titles /api/public/v1/maintenance/past '.items[]' \
  'Database upgrade,Network maintenance,Certificate rotation,Storage migration'
expect_titles /api/public/v1/maintenance '.active[]' 'Load balancer swap'
expect_titles /api/public/v1/maintenance '.upcoming[]' 'Kernel patching'

echo "==> Status page"
expect_page '/' has 'History (30 days)'
expect_page '/' has 'Database upgrade'
expect_page '/' has 'Checkout errors (seeded)'
expect_page '/' has 'Network maintenance'
expect_page '/' lacks 'Storage migration'
expect_page '/' lacks 'Cancelled window'
expect_page '/' lacks 'Back office move'
expect_history '/' has 'Checkout errors (seeded)'
expect_history '/' has 'Database upgrade'
expect_history '/?show=incidents' has 'Checkout errors (seeded)'
expect_history '/?show=incidents' lacks 'Database upgrade'
expect_history '/?show=maintenance' has 'Database upgrade'
expect_history '/?show=maintenance' lacks 'Checkout errors (seeded)'
expect_page '/?show=maintenance' has 'fragment=1&#38;show=maintenance'

echo "==> Archive"
expect_page '/status/incidents' has 'Storage migration'
expect_page '/status/incidents' has 'Slow API responses (seeded)'
expect_page '/status/incidents?show=maintenance' has 'Maintenance history'
expect_page '/status/incidents?show=maintenance' has 'In progress'
expect_page '/status/incidents?show=maintenance' lacks 'Kernel patching'
expect_page '/status/incidents?show=maintenance' lacks 'Cancelled window'
expect_page '/status/incidents?show=maintenance' lacks 'Slow API responses (seeded)'
expect_page '/status/incidents?show=incidents' lacks 'Storage migration'

echo
if (( failed )); then
  echo "==> Some checks failed."
  exit 1
fi
echo "==> All checks passed."
