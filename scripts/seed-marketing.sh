#!/usr/bin/env bash
# Seed a photogenic operator tenant — 16 monitors across 3 groups, 90d of
# mostly-up ClickHouse history, two resolved incidents — for shooting the
# static/marketing/shot-*.webp gallery on the landing page.
#
# Distinct from seed-fixtures.sh, which exists to stress every render path and
# so is deliberately hostile: major outages, NoData gaps, adversarial titles,
# disabled rows. That set proves the UI survives; this set makes it look like
# software people run. Neither can serve the other's goal, hence two scripts.
#
# Three constraints drive the shape here, all of them things the UI does:
#
#   1. Tags render as chips (targets/partials/_row.html). So the wipe key
#      can't be a tag the way seed-fixtures uses `seed-fixtures` — it would
#      photograph on every row. Wipe is org-scoped instead, and tags carry
#      only content worth seeing.
#   2. enabled=false renders as paused (same file, data-paused). seed-fixtures
#      disables rows to stop the scheduler clobbering seeded counters; here
#      that would paint every monitor paused. Rows stay enabled and the
#      SCHEDULER is turned off instead (see below).
#   3. With no scheduler, nothing is ever probed, so hosts are set dressing:
#      real-looking uptimepage.dev subdomains rather than reachable mocks.
#
# Nothing here may be probed, or the seeded state is overwritten within one
# interval. Two probers exist and only one is already handled:
#
#   * The in-process scheduler — compose.dev.yml already sets
#     UPTIMEPAGE_SCHEDULER__ENABLED=false, so the dev stack needs no change.
#     A stack built another way does (the config default is ON).
#   * `just dev-regions` agents — these DO probe. Stop them first; the script
#     refuses to run while they are up.
#
# REQUIRED, and the catch: stopping the agents trips the dead-man's switch.
# Once no agent has reported within operator.agent_stale_after_secs (default
# 90), the silence sweeper marks every assigned monitor unmonitored and the
# public page renders each component grey "No data" instead of Operational.
# The seed seeds a placeholder agent per enabled region and bumps last_seen_at
# (dev-regions-down deletes the real rows, so a bump alone hits nothing), but
# that freshness only lasts 90s — so run the app with a window wide enough:
#
#   UPTIMEPAGE_OPERATOR__AGENT_STALE_AFTER_SECS=86400
#
# Do NOT put that in compose.dev.yml: a 24h window blinds the switch to
# genuinely dead agents during normal dev. It is a shoot-time override.
#
# The incident writer is a third actor and cannot be switched off, but it only
# reacts to states this profile no longer seeds. See the search-api block.
#
# Heartbeat and browser flow need no seeded monitors: the new-monitor form's
# type rail lists every check type, so that shot is /targets/new, viewed only.
# Nothing is created there.
#
# Env overrides:
#   SLUG          org slug to seed onto       (default: uptimepage)
#   PG_CONTAINER  postgres container name     (default: uptimepage-postgres-1)
#   CH_CONTAINER  clickhouse container name   (default: uptimepage-clickhouse-1)
#   BASE_DOMAIN   for the printed URL         (default: lvh.me)
#   RESET_CH      0 = skip purge; the CH inserts are not idempotent on
#                 timestamp, so default 1 purges the org first. (default: 1)
#
# Requires `just up-app` + `SLUG=… just dev-login` first (org must exist).
set -euo pipefail

SLUG="${SLUG:-uptimepage}"
PG_CONTAINER="${PG_CONTAINER:-uptimepage-postgres-1}"
CH_CONTAINER="${CH_CONTAINER:-uptimepage-clickhouse-1}"
BASE_DOMAIN="${BASE_DOMAIN:-lvh.me}"
RESET_CH="${RESET_CH:-1}"

pg() { docker exec -i "$PG_CONTAINER" psql -U monitor -d monitor -v ON_ERROR_STOP=1 "$@"; }
ch() { docker exec -i "$CH_CONTAINER" clickhouse-client "$@"; }

if ! pg -tAc "SELECT 1 FROM organizations WHERE slug='${SLUG}'" | grep -q 1; then
  echo "error: org '${SLUG}' missing — run 'SLUG=${SLUG} just dev-login' first" >&2
  exit 1
fi

# A live agent probes the seeded targets and flips them down within one
# interval, which is exactly the failure this whole profile exists to avoid.
if docker ps --format '{{.Names}}' | grep -q '^uptimepage-agent-'; then
  echo "error: dev-region agents are running — they will probe these hosts and" >&2
  echo "       overwrite the seeded state. Stop them first:" >&2
  echo "         docker stop \$(docker ps -q --filter name=uptimepage-agent-)" >&2
  exit 1
fi

echo "==> Postgres: reset marketing tenant"
pg <<SQL
UPDATE organizations SET name = 'Uptimepage' WHERE slug = '${SLUG}';

-- Org-scoped wipe: no tag key, because tags are on camera. incident_updates,
-- incident_events and target_regions all cascade via FK.
DELETE FROM incidents
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}');
DELETE FROM targets
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}');
DELETE FROM notification_channels
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}');
DELETE FROM status_pages
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}');

-- Page slug must equal the subdomain label (= org slug): the public surface
-- resolves {slug}.{base_domain} against status_pages.slug directly.
INSERT INTO status_pages
  (org_id, slug, name, enabled, public_display_name, public_about, public_brand_color)
SELECT id, '${SLUG}', 'Uptimepage Status', true, 'Uptimepage Status',
       'Live status of the Uptimepage API, dashboard, and status pages.',
       '#16a34a'
  FROM organizations WHERE slug = '${SLUG}';
SQL

OWNER_USER_ID=$(pg -tAc \
  "SELECT m.user_id::text FROM memberships m
     JOIN users u ON u.id = m.user_id AND u.deleted_at IS NULL
    WHERE m.org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
    ORDER BY m.created_at ASC LIMIT 1;" 2>/dev/null || true)
if [[ -z "$OWNER_USER_ID" ]]; then
  echo "    (no membership found — every monitor will render unowned)"
fi

echo "==> Postgres: insert 40 monitors across 4 groups (10 public components)"
# p0/p1 tags get a coloured dot + their own chip class in _row.html; they earn
# their place on camera. No write_source updates — the TERRAFORM/API badges
# read as synthetic in a marketing shot.
pg <<SQL
WITH org AS (SELECT id FROM organizations WHERE slug='${SLUG}'),
     sp AS (SELECT id FROM status_pages
             WHERE org_id = (SELECT id FROM org) AND slug = '${SLUG}'),
     spec(name,public,pname,grp,so,gname,tags,has_owner,spec) AS (VALUES
  ('checkout-api', true, 'API', 'Core Services', 0, 'Production', ARRAY['p0','api'], true,
   '{"type":"http","url":"https://api.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('web-frontend', true, 'Website', 'Core Services', 1, 'Production', ARRAY['p0','web'], true,
   '{"type":"http","url":"https://uptimepage.dev/","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('graphql-gateway', true, 'GraphQL API', 'Core Services', 2, 'Production', ARRAY['p0','api'], true,
   '{"type":"http","url":"https://api.uptimepage.dev/graphql","method":"POST","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('auth-service', true, 'Authentication', 'Core Services', 3, 'Production', ARRAY['p1','api'], true,
   '{"type":"http","url":"https://auth.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('search-api', true, 'Search', 'Core Services', 4, 'Production', ARRAY['api'], false,
   '{"type":"http","url":"https://search.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('billing-api', false, NULL, NULL, 0, 'Production', ARRAY['p1','api'], true,
   '{"type":"http","url":"https://billing.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('payments-webhook', false, NULL, NULL, 0, 'Production', ARRAY['api'], true,
   '{"type":"http","url":"https://api.uptimepage.dev/webhooks/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('notifications-api', false, NULL, NULL, 0, 'Production', ARRAY['api'], true,
   '{"type":"http","url":"https://notify.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('email-delivery', true, 'Email Delivery', 'Notifications', 0, 'Production', ARRAY['email'], true,
   '{"type":"http","url":"https://mail.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('media-uploads', false, NULL, NULL, 0, 'Production', ARRAY['api'], false,
   '{"type":"http","url":"https://uploads.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('exports-worker', false, NULL, NULL, 0, 'Production', ARRAY['worker'], false,
   '{"type":"http","url":"https://exports.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('admin-api', false, NULL, NULL, 0, 'Production', ARRAY['api'], false,
   '{"type":"http","url":"https://admin.uptimepage.dev/api/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('postgres-primary', true, 'Database', 'Infrastructure', 0, 'Infrastructure', ARRAY['p0','db'], true,
   '{"type":"tcp","host":"db-primary.uptimepage.dev","port":5432,"timeout":3000}'),
  ('postgres-replica', false, NULL, NULL, 0, 'Infrastructure', ARRAY['db'], true,
   '{"type":"tcp","host":"db-replica.uptimepage.dev","port":5432,"timeout":3000}'),
  ('clickhouse-analytics', false, NULL, NULL, 0, 'Infrastructure', ARRAY['db'], false,
   '{"type":"tcp","host":"analytics.uptimepage.dev","port":8123,"timeout":3000}'),
  ('redis-cache', false, NULL, NULL, 0, 'Infrastructure', ARRAY['cache'], false,
   '{"type":"tcp","host":"cache.uptimepage.dev","port":6379,"timeout":3000}'),
  ('redis-sessions', false, NULL, NULL, 0, 'Infrastructure', ARRAY['cache'], false,
   '{"type":"tcp","host":"sessions.uptimepage.dev","port":6379,"timeout":3000}'),
  ('message-queue', false, NULL, NULL, 0, 'Infrastructure', ARRAY['p1','queue'], true,
   '{"type":"tcp","host":"queue.uptimepage.dev","port":5672,"timeout":3000}'),
  ('object-storage', true, 'Object Storage', 'Infrastructure', 1, 'Infrastructure', ARRAY['storage'], false,
   '{"type":"http","url":"https://assets.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('metrics-collector', false, NULL, NULL, 0, 'Infrastructure', ARRAY['ops'], false,
   '{"type":"http","url":"https://metrics.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('backup-runner', false, NULL, NULL, 0, 'Infrastructure', ARRAY['ops'], false,
   '{"type":"http","url":"https://backup.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('internal-admin', false, NULL, NULL, 0, 'Infrastructure', ARRAY['web'], false,
   '{"type":"http","url":"https://admin.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('cdn-edge', true, 'CDN', 'Edge', 0, 'Edge', ARRAY['edge'], true,
   '{"type":"http","url":"https://cdn.uptimepage.dev/app.css","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('status-page', true, 'Status Page', 'Edge', 1, 'Edge', ARRAY['web'], true,
   '{"type":"http","url":"https://status.uptimepage.dev/","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('edge-eu', false, NULL, NULL, 0, 'Edge', ARRAY['edge'], false,
   '{"type":"http","url":"https://eu.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('edge-us', false, NULL, NULL, 0, 'Edge', ARRAY['edge'], false,
   '{"type":"http","url":"https://us.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('waf-endpoint', false, NULL, NULL, 0, 'Edge', ARRAY['edge'], false,
   '{"type":"http","url":"https://waf.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('dns-apex', false, NULL, NULL, 0, 'Edge', ARRAY['dns'], false,
   '{"type":"dns","domain":"uptimepage.dev","record_type":"A","resolver":"1.1.1.1","expected_contains":null,"timeout":3000}'),
  ('dns-mx', false, NULL, NULL, 0, 'Edge', ARRAY['dns'], false,
   '{"type":"dns","domain":"uptimepage.dev","record_type":"MX","resolver":"1.1.1.1","expected_contains":null,"timeout":3000}'),
  ('tls-cert-apex', false, NULL, NULL, 0, 'Edge', ARRAY['tls'], false,
   '{"type":"tls_cert","host":"uptimepage.dev","port":443,"server_name":null,"warn_days":30,"critical_days":7,"timeout":5000}'),
  ('tls-cert-api', false, NULL, NULL, 0, 'Edge', ARRAY['tls'], false,
   '{"type":"tls_cert","host":"api.uptimepage.dev","port":443,"server_name":null,"warn_days":30,"critical_days":7,"timeout":5000}'),
  ('public-api', true, 'Public API', 'Integrations', 0, 'Integrations', ARRAY['api'], true,
   '{"type":"http","url":"https://api.uptimepage.dev/v1/status","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('mcp-server', false, NULL, NULL, 0, 'Integrations', ARRAY['api'], true,
   '{"type":"http","url":"https://mcp.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('stripe-webhook', false, NULL, NULL, 0, 'Integrations', ARRAY['p1','webhook'], true,
   '{"type":"http","url":"https://api.uptimepage.dev/hooks/stripe","method":"POST","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('github-webhook', false, NULL, NULL, 0, 'Integrations', ARRAY['webhook'], false,
   '{"type":"http","url":"https://api.uptimepage.dev/hooks/github","method":"POST","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('slack-app', false, NULL, NULL, 0, 'Integrations', ARRAY['webhook'], false,
   '{"type":"http","url":"https://api.uptimepage.dev/hooks/slack","method":"POST","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('telegram-bot', false, NULL, NULL, 0, 'Integrations', ARRAY['webhook'], false,
   '{"type":"http","url":"https://api.uptimepage.dev/hooks/telegram","method":"POST","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('terraform-registry', false, NULL, NULL, 0, 'Integrations', ARRAY['api'], false,
   '{"type":"http","url":"https://registry.uptimepage.dev/health","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('docs-site', false, NULL, NULL, 0, 'Integrations', ARRAY['web'], false,
   '{"type":"http","url":"https://docs.uptimepage.dev/","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('domain-expiry', false, NULL, NULL, 0, 'Integrations', ARRAY['domain'], false,
   '{"type":"domain_expiry","domain":"uptimepage.dev","warn_days":60,"critical_days":14,"timeout":10000}')
     ),
     ins AS (
       INSERT INTO targets
         (org_id, name, check_spec, interval_secs, enabled, tags, group_name, owner_user_id)
       SELECT org.id, spec.name, spec.spec::jsonb,
              CASE WHEN spec.spec::jsonb->>'type' IN ('tls_cert','domain_expiry') THEN 86400 ELSE 60 END,
              true, spec.tags, spec.gname,
              CASE WHEN spec.has_owner AND '${OWNER_USER_ID}' <> ''
                   THEN '${OWNER_USER_ID}'::uuid ELSE NULL END
       FROM org, spec
       RETURNING id, name
     )
INSERT INTO status_page_components
  (org_id, status_page_id, target_id, public_name, public_group, sort_order)
SELECT (SELECT id FROM org), (SELECT id FROM sp), ins.id, spec.pname, spec.grp, spec.so
FROM ins JOIN spec ON spec.name = ins.name
WHERE spec.public;
SQL

ORG=$(pg -tAc "SELECT id FROM organizations WHERE slug='${SLUG}';")
: "${ORG:?org id query returned empty}"

# name -> uuid map. Positional `read -r` does not scale past a handful of
# monitors: string_agg sorts an unknown name LAST (array_position -> NULL ->
# NULLS LAST) and silently shifts every assignment by one.
declare -A TID
while IFS=$'\t' read -r _n _i; do TID["$_n"]="$_i"; done < <(
  pg -tAc "SELECT name || E'\t' || id FROM targets WHERE org_id='${ORG}';")

EXPECTED_TARGETS=40
if (( ${#TID[@]} != EXPECTED_TARGETS )); then
  echo "error: expected ${EXPECTED_TARGETS} targets, found ${#TID[@]} — a name in the" >&2
  echo "       VALUES block is duplicated or the insert failed." >&2
  exit 1
fi
for _n in "${!TID[@]}"; do
  if [[ ! "${TID[$_n]}" =~ ^[0-9a-f-]{36}$ ]]; then
    echo "error: target '${_n}' resolved to '${TID[$_n]}', not a uuid." >&2
    exit 1
  fi
done

# Named handles for the monitors the history/incident blocks below single out.
T_SEARCH="${TID[search-api]}"
T_CDN="${TID[cdn-edge]}"
for _v in T_SEARCH T_CDN; do
  [[ -n "${!_v}" ]] || { echo "error: ${_v} unset — monitor renamed?" >&2; exit 1; }
done

mapfile -t SEED_REGIONS < <(pg -tAc "SELECT id FROM regions WHERE enabled ORDER BY id")
if (( ${#SEED_REGIONS[@]} == 0 )); then
  echo "error: no enabled regions — start the stack (and 'just dev-regions' once) first" >&2
  exit 1
fi
echo "    regions: ${SEED_REGIONS[*]}"
CH_REGIONS=$(printf "'%s'," "${SEED_REGIONS[@]}"); CH_REGIONS="${CH_REGIONS%,}"
REGS_ALL="ARRAY[${CH_REGIONS}]::text[]"
REGS_DOWN_ONE="ARRAY['${SEED_REGIONS[0]}']::text[]"
if (( ${#SEED_REGIONS[@]} > 1 )); then
  _rest=$(printf "'%s'," "${SEED_REGIONS[@]:1}"); _rest="${_rest%,}"
  REGS_UP_REST="ARRAY[${_rest}]::text[]"
else
  REGS_UP_REST="ARRAY[]::text[]"
fi

echo "==> Postgres: assign every monitor to every enabled region"
pg <<SQL
INSERT INTO target_regions (target_id, region)
SELECT t.id, r.id FROM targets t CROSS JOIN regions r
 WHERE t.org_id = '${ORG}' AND r.enabled
ON CONFLICT DO NOTHING;
SQL

echo "==> Postgres: refresh agent liveness (dead-man's switch)"
# The silence sweeper marks a monitor unmonitored unless a live agent row covers
# its region — the public page then renders a grey "No data" per component
# rather than claiming up. Correct behaviour, fatal to the shot. dev-regions-down
# deletes the agent rows outright, so bumping last_seen_at is not enough: seed a
# placeholder that probes nothing (a bare row starts no prober) per enabled
# region. The bump then buys operator.agent_stale_after_secs (default 90s), so
# the shoot stack also needs a wide window; see the header.
pg <<SQL
INSERT INTO agents (region, name, enabled, token_hash, token_prefix, last_seen_at)
SELECT r.id, 'marketing-seed', true,
       'seed-placeholder', 'seedmktg' || left(md5(r.id), 8), now()
  FROM regions r
 WHERE r.enabled
   AND NOT EXISTS (SELECT 1 FROM agents a WHERE a.region = r.id AND a.enabled);

UPDATE agents SET last_seen_at = now() WHERE enabled;
UPDATE monitor_silence_state SET resolved_at = now()
 WHERE org_id = '${ORG}' AND resolved_at IS NULL;
SQL

if [[ "$RESET_CH" == "1" ]]; then
  echo "==> ClickHouse: purge prior rows for this org (raw + both rollups)"
  # Deleting from check_results does not cascade into the matviews built off
  # it, and rollup_source() reads _1h for any window past MINUTE_ROLLUP_DAYS —
  # skipping them stacks another copy of every seeded outage per rerun.
  for tbl in check_results check_results_1m check_results_1h; do
    ch -q "ALTER TABLE monitor.${tbl} DELETE WHERE org_id = toUUID('${ORG}') SETTINGS mutations_sync = 2"
  done
fi

# 60s for the last 24h drives the counters, sparklines and 'last check'; 15-min
# back to 90d drives the day strip at a fraction of the rows. Rows run up to
# now() (seed-fixtures shifts 6h back instead) so the status classifier reads
# the board live with no scheduler running. Raw is TTL'd to 30d; the 90d view
# renders from the _1h rollup, which the matviews fill at insert time.
# Daily-interval monitors get their own sparse series below; everything else
# takes the dense 60s/15min treatment.
SPARSE_NAMES=(tls-cert-apex tls-cert-api domain-expiry)
DENSE_TARGETS=()
for _n in "${!TID[@]}"; do
  _skip=0
  for _s in "${SPARSE_NAMES[@]}"; do [[ "$_n" == "$_s" ]] && _skip=1; done
  (( _skip )) || DENSE_TARGETS+=("${TID[$_n]}")
done
SPARSE_TARGETS=()
for _s in "${SPARSE_NAMES[@]}"; do SPARSE_TARGETS+=("${TID[$_s]}"); done


echo "==> ClickHouse: 90d up history for ${#DENSE_TARGETS[@]} monitors (60s last 24h, 15min to 90d)"
for tid in "${DENSE_TARGETS[@]}"; do
  ch -mn <<SQL
INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${tid}'),r,
       now() - toIntervalMinute(number),
       'up',
       90 + (cityHash64('${tid}') % 80) + multiIf(r = 'eu-helsinki', 0, r = 'us-east', 85, r = 'apac-sg', 170, cityHash64(r) % 140)
          + toUInt32(25 + 25 * sin(number / 45.0))
          + (rand(number) % 20),
       200
FROM numbers(1440) ARRAY JOIN [${CH_REGIONS}] AS r;

INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${tid}'),r,
       now() - toIntervalDay(1) - toIntervalMinute(number * 15),
       'up',
       90 + (cityHash64('${tid}') % 80) + multiIf(r = 'eu-helsinki', 0, r = 'us-east', 85, r = 'apac-sg', 170, cityHash64(r) % 140)
          + toUInt32(25 + 25 * sin(number / 12.0))
          + (rand(number) % 20),
       200
FROM numbers(8544) ARRAY JOIN [${CH_REGIONS}] AS r;
SQL
done

echo "==> ClickHouse: daily history for the cert + domain monitors"
# interval_secs=86400 on these, so a 60s series would contradict the monitor's
# own config on the detail page. Last row ~2h old: correct for a daily check.
for tid in "${SPARSE_TARGETS[@]}"; do
  ch -mn <<SQL
INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${tid}'),r,
       now() - toIntervalHour(2) - toIntervalDay(number),
       'up',
       40 + (cityHash64('${tid}') % 30) + multiIf(r = 'eu-helsinki', 0, r = 'us-east', 25, r = 'apac-sg', 55, cityHash64(r) % 50) + (rand(number) % 10),
       200
FROM numbers(90) ARRAY JOIN [${CH_REGIONS}] AS r;
SQL
done

echo "==> ClickHouse: search-api — resolved dip 3 days ago"
# Nothing is left live-degraded: the incident writer opens an untitled `major`
# incident off all-region degraded rows, flipping the public Search component
# out of Operational. Non-green texture comes from resolved history instead.
ch -mn <<SQL
INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${T_SEARCH}'),r,
       now() - toIntervalMinute(number),
       'up',
       110 + multiIf(r = 'eu-helsinki', 0, r = 'us-east', 85, r = 'apac-sg', 170, cityHash64(r) % 140) + toUInt32(20 + 20 * sin(number / 40.0)) + (rand(number) % 15),
       200
FROM numbers(1440) ARRAY JOIN [${CH_REGIONS}] AS r;

INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${T_SEARCH}'),r,
       now() - toIntervalDay(1) - toIntervalMinute(number * 15),
       'up',
       110 + multiIf(r = 'eu-helsinki', 0, r = 'us-east', 85, r = 'apac-sg', 170, cityHash64(r) % 140) + toUInt32(20 + 20 * sin(number / 12.0)) + (rand(number) % 15),
       200
FROM numbers(8544) ARRAY JOIN [${CH_REGIONS}] AS r
WHERE NOT (number BETWEEN 192 AND 194);

-- Resolved 45-minute dip 3 days back. Deliberately outside the 24h window:
-- inside it, the dashboard's 24h strip paints the fully-degraded bucket red
-- and its partial neighbour amber, which is the only non-green on an otherwise
-- healthy board. It still shows up in 7d/30d/90d and in 30d uptime, so the
-- history stays honest without a scar on the hero shot.
-- numbers(2880, 45) is the offset form (yields 2880..2924); plain numbers(45)
-- would yield 0..44 and silently insert nothing.
INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${T_SEARCH}'),r,
       now() - toIntervalMinute(number),
       'degraded',
       820 + multiIf(r = 'eu-helsinki', 0, r = 'us-east', 85, r = 'apac-sg', 170, cityHash64(r) % 140) + (rand(number) % 300),
       200
FROM numbers(2880, 45) ARRAY JOIN [${CH_REGIONS}] AS r;
SQL

echo "==> ClickHouse: cdn-edge — resolved outage 3 weeks ago"
ch -mn <<SQL
ALTER TABLE monitor.check_results DELETE
 WHERE org_id = toUUID('${ORG}') AND target_id = toUUID('${T_CDN}')
   AND timestamp BETWEEN now() - toIntervalDay(21) - toIntervalMinute(25)
                     AND now() - toIntervalDay(21)
SETTINGS mutations_sync = 2;

INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code,error)
SELECT toUUID('${ORG}'),toUUID('${T_CDN}'),r,
       now() - toIntervalDay(21) - toIntervalMinute(number),
       'down', 0, 502, 'no response'
FROM numbers(25) ARRAY JOIN [${CH_REGIONS}] AS r;
SQL

echo "==> Postgres: 2 resolved incidents"
pg <<SQL
WITH ins AS (
  INSERT INTO incidents
    (org_id, target_id, started_at, ended_at, severity, status_at_start,
     check_count, error_sample, public_title, public_description, duration_secs,
     state, visibility, origin, regions_down, regions_up)
  VALUES
    ('${ORG}'::uuid, '${T_SEARCH}'::uuid,
     now() - interval '2924 minutes', now() - interval '2880 minutes',
     'minor', 'degraded', 45, 'slow upstream response',
     'Slow search responses',
     'Search queries were returning slowly for about 45 minutes. A saturated upstream index node was rotated out.',
     2700, 'resolved', 'public', 'monitor', ${REGS_DOWN_ONE}, ${REGS_UP_REST}),
    ('${ORG}'::uuid, '${T_CDN}'::uuid,
     now() - interval '21 days' - interval '25 minutes', now() - interval '21 days',
     'major', 'down', 25, 'no response',
     'CDN edge unreachable',
     'A CDN edge PoP stopped serving assets. Traffic was shifted to the neighbouring PoP while the origin config was rolled back.',
     1500, 'resolved', 'public', 'monitor', ${REGS_ALL}, ARRAY[]::text[])
  RETURNING id, started_at, ended_at
)
INSERT INTO incident_updates (org_id, incident_id, posted_at, phase, message, author)
SELECT '${ORG}', i.id, p.posted_at, p.phase, p.message, NULLIF('${OWNER_USER_ID}', '')::uuid
FROM ins i
CROSS JOIN LATERAL (
  SELECT i.started_at AS posted_at, 'investigating' AS phase,
         'Investigating elevated response times.' AS message
  UNION ALL SELECT i.started_at + (i.ended_at - i.started_at)*0.35,
         'identified', 'Root cause identified.'
  UNION ALL SELECT i.started_at + (i.ended_at - i.started_at)*0.70,
         'monitoring', 'Mitigation applied; monitoring.'
  UNION ALL SELECT i.ended_at,
         'resolved', 'Service fully restored.'
) p;
SQL

echo "==> Postgres: 10 notification channels"
# EmailConfig.to is a single String ("a second address is a second channel"), not
# an array — an array reaches the list endpoint as a 500, "decode channel config:
# invalid type: sequence, expected a string".
#
# created_at is set per row: the column defaults to now(), so a bulk insert
# stamps all ten with an identical second and the CREATED column photographs as
# a fixture import. Each channel instead dates from when a team would have added
# it — chat first, paging once on-call existed, ntfy last.
pg <<SQL
INSERT INTO notification_channels
  (org_id, name, kind, config, external_ref, enabled, disabled_reason, verified_at,
   created_at, updated_at) VALUES
  ('${ORG}'::uuid, 'Engineering Slack', 'slack',
   '{"type":"slack","webhook_url":"https://hooks.slack.com/services/T0000/B0000/XXXXXXXXXXXXXXXXXXXXXXXX"}'::jsonb,
   NULL, true, NULL, now() - interval '58 days 3 hours',
   now() - interval '58 days 4 hours', now() - interval '58 days 3 hours'),
  ('${ORG}'::uuid, 'Ops email', 'email',
   '{"type":"email","to":"ops@uptimepage.dev"}'::jsonb,
   NULL, true, NULL, now() - interval '57 days 6 hours',
   now() - interval '57 days 7 hours', now() - interval '57 days 6 hours'),
  ('${ORG}'::uuid, 'On-call PagerDuty', 'pagerduty',
   '{"type":"pagerduty","routing_key":"00000000000000000000000000000000"}'::jsonb,
   NULL, true, NULL, now() - interval '52 days 2 hours',
   now() - interval '52 days 3 hours', now() - interval '52 days 2 hours'),
  ('${ORG}'::uuid, 'On-call SMS', 'sms',
   '{"type":"sms","provider":"twilio","to":"+358401234567","from":"+14155550142","account_sid":"ACxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx","auth_token":"fixture-twilio-auth-token"}'::jsonb,
   NULL, true, NULL, now() - interval '51 days 5 hours',
   now() - interval '51 days 6 hours', now() - interval '51 days 5 hours'),
  ('${ORG}'::uuid, 'Runbook automation', 'webhook',
   '{"type":"webhook","url":"https://hooks.internal.uptimepage.dev/uptime","headers":{"X-Source":"uptimepage"},"secret":"fixture-webhook-signing-secret"}'::jsonb,
   NULL, true, NULL, now() - interval '44 days 1 hour',
   now() - interval '44 days 2 hours', now() - interval '44 days 1 hour'),
  ('${ORG}'::uuid, 'Community Discord', 'discord',
   '{"type":"discord","webhook_url":"https://discord.com/api/webhooks/000000000000000000/XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"}'::jsonb,
   NULL, true, NULL, now() - interval '39 days 8 hours',
   now() - interval '39 days 9 hours', now() - interval '39 days 8 hours'),
  ('${ORG}'::uuid, 'Platform Teams', 'msteams',
   '{"type":"msteams","webhook_url":"https://uptimepage.webhook.office.com/webhookb2/00000000-0000-0000-0000-000000000000@00000/IncomingWebhook/0000000000000000/00000000-0000-0000-0000-000000000000"}'::jsonb,
   NULL, true, NULL, now() - interval '22 days 4 hours',
   now() - interval '22 days 5 hours', now() - interval '22 days 4 hours'),
  -- Disabled with a reason: the operator UI renders the state + why. updated_at
  -- is recent because unlinking is what last touched it.
  ('${ORG}'::uuid, 'Status bot', 'telegram_app',
   '{"type":"telegram_app","chat_id":"-1002104857392","chat_title":"Uptimepage Ops"}'::jsonb,
   '-1002104857392', false, 'unlinked from the Telegram side', NULL,
   now() - interval '19 days 2 hours', now() - interval '6 days 3 hours'),
  ('${ORG}'::uuid, 'Ops phone', 'pushover',
   '{"type":"pushover","token":"fixture-pushover-app-token","user":"fixture-pushover-user-key","emergency":true}'::jsonb,
   NULL, true, NULL, now() - interval '17 days 7 hours',
   now() - interval '17 days 8 hours', now() - interval '17 days 7 hours'),
  ('${ORG}'::uuid, 'Self-hosted ntfy', 'ntfy',
   '{"type":"ntfy","server_url":"https://ntfy.uptimepage.dev","topic":"uptime-alerts","access_token":"tk_fixture-ntfy-token"}'::jsonb,
   NULL, true, NULL, now() - interval '9 days 3 hours',
   now() - interval '9 days 4 hours', now() - interval '9 days 3 hours');
SQL

echo "==> Postgres: route monitors to channels"
# Derived from tags/group rather than a per-monitor list: a name list silently
# leaves new monitors unrouted, and an unrouted channel photographs as an unused
# install. Priority tags win over group, so p0 pages a human wherever it lives.
pg <<SQL
WITH org AS (SELECT id FROM organizations WHERE slug='${SLUG}'),
     chan AS (SELECT name, id FROM notification_channels
               WHERE org_id = (SELECT id FROM org))
UPDATE targets t SET
  -- jsonb_agg over zero rows returns NULL, which violates alerts NOT NULL.
  alerts = COALESCE((
    SELECT jsonb_agg(jsonb_build_object('channel_id', c.id))
      FROM chan c
     WHERE c.name = ANY(CASE
       WHEN t.tags @> ARRAY['p0']     THEN ARRAY['Engineering Slack','On-call PagerDuty','On-call SMS']
       WHEN t.tags @> ARRAY['p1']     THEN ARRAY['Engineering Slack','On-call PagerDuty']
       WHEN t.tags @> ARRAY['tls']
         OR t.tags @> ARRAY['domain'] THEN ARRAY['Ops email','Engineering Slack']
       WHEN t.tags @> ARRAY['db']     THEN ARRAY['Engineering Slack','Runbook automation','Ops phone']
       WHEN t.tags @> ARRAY['email']  THEN ARRAY['Engineering Slack','Ops email']
       WHEN t.tags @> ARRAY['ops']    THEN ARRAY['Self-hosted ntfy','Engineering Slack']
       WHEN t.group_name = 'Edge'         THEN ARRAY['Engineering Slack','Community Discord']
       WHEN t.group_name = 'Integrations' THEN ARRAY['Engineering Slack','Platform Teams']
       WHEN t.group_name = 'Infrastructure' THEN ARRAY['Engineering Slack','Runbook automation']
       ELSE ARRAY['Engineering Slack']
     END)
  ), '[]'::jsonb),
  alert_confirmations = CASE
    WHEN t.tags @> ARRAY['p0'] THEN 2
    WHEN t.tags @> ARRAY['p1'] THEN 2
    WHEN t.tags @> ARRAY['tls'] OR t.tags @> ARRAY['domain'] THEN 1
    ELSE 3 END,
  notify_recovery = true,
  renotify_interval_secs = CASE
    WHEN t.tags @> ARRAY['p0'] THEN 900
    WHEN t.tags @> ARRAY['p1'] THEN 1800
    WHEN t.tags @> ARRAY['tls'] OR t.tags @> ARRAY['domain'] THEN 86400
    WHEN t.group_name = 'Edge' THEN 1800
    ELSE 3600 END
WHERE t.org_id = (SELECT id FROM org);
SQL

echo
echo "Seeded '${SLUG}'."
echo "  operator:    http://app.${BASE_DOMAIN}:8080/targets"
echo "  dashboard:   http://app.${BASE_DOMAIN}:8080/dashboard"
echo "  check types: http://app.${BASE_DOMAIN}:8080/targets/new  (view only, covers heartbeat and browser flow)"
echo "  status page: http://${SLUG}.${BASE_DOMAIN}:8080/"
echo
echo "Keep the dev-region agents stopped until the shots are taken — they probe"
echo "these hosts and will turn the board red. Restart them after:"
echo "  docker start \$(docker ps -aq --filter name=uptimepage-agent-)"
