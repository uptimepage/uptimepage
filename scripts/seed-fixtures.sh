#!/usr/bin/env bash
# Seed a substantial fixture set (14 monitors, ~160 incidents across all
# statuses, 90d ClickHouse history, notification channels, alert bindings,
# maintenance-bound components, adversarial-title incident) into a running local
# stack — for UI stress-testing, screenshots, and smoke-testing every rendered
# code path on the public + operator pages.
#
# Regions are NOT invented here: every fixture monitor is assigned to whatever
# enabled regions already exist (the control plane's home region plus any real
# agents from `just dev-regions`), and the ClickHouse history is written into
# each of those regions. Bring the stack + regions up first, then seed; the
# region selectors, per-region breakdown/overlay, and per-region history then
# reflect the real topology. Incidents also carry a regions_down/up breakdown —
# "Partial regional outage" rows mark one region down with the rest up; the rest
# read as full all-region outages.
#
# Coverage matrix on the public page — states derive from CONFIRMED open
# public incidents (+ maintenance), never from raw check rows:
#   fix-api    Operational   (raw error blips in the drawer, NO incident —
#                             the page must stay green; plus a resolved
#                             incident ending exactly at last midnight, so
#                             yesterday's strip cell paints and today's not)
#   fix-web    Degraded      (open incident, status_at_start='degraded')
#   fix-cdn    PartialOutage (open incident, one region down, rest up)
#   fix-db     Maintenance   (bound to the active maintenance window)
#   fix-auth   MajorOutage   (open incident, every region down)
#   fix-email  Operational + NoData history-gap days (skipped writes)
#   fix-search Operational, no group (renders ungrouped path)
#   fix-paused Degraded via a PUBLISHED MANUAL minor incident (severity →
#              impact mapping) + disabled-target render edge case
# Operator-only:
#   fix-payment http internal, fix-admin http internal,
#   fix-tcp tcp, fix-dns dns, fix-tls tls_cert, fix-domain domain_expiry.
#
# Idempotent: rows are tagged `seed-fixtures` and wiped before re-insert
# so re-running gives the same shape without duplicates. ClickHouse rows
# for the seeded targets are only purged when RESET_CH=1 (ALTER … DELETE
# is async + heavy; on a throwaway stack `just down-clean` is cheaper).
#
# Env overrides:
#   SLUG          org slug to seed onto       (default: devorg)
#   PG_CONTAINER  postgres container name     (default: uptimepage-postgres-1)
#   CH_CONTAINER  clickhouse container name   (default: uptimepage-clickhouse-1)
#   BASE_DOMAIN   for the printed URL         (default: lvh.me)
#   RESET_CH      0 = skip purge (CH rows from prior runs accumulate); the
#                 ClickHouse inserts below are NOT idempotent on timestamp,
#                 so the default is 1 — purge before re-insert. Set to 0
#                 only when you want to layer additional rows on top of an
#                 existing seed. (default: 1)
#
# Requires `just up-app` + `just dev-login` first (org must already exist).
set -euo pipefail

SLUG="${SLUG:-devorg}"
PG_CONTAINER="${PG_CONTAINER:-uptimepage-postgres-1}"
CH_CONTAINER="${CH_CONTAINER:-uptimepage-clickhouse-1}"
BASE_DOMAIN="${BASE_DOMAIN:-lvh.me}"
RESET_CH="${RESET_CH:-1}"

pg() { docker exec -i "$PG_CONTAINER" psql -U monitor -d monitor -v ON_ERROR_STOP=1 "$@"; }
ch() { docker exec -i "$CH_CONTAINER" clickhouse-client "$@"; }

if ! pg -tAc "SELECT 1 FROM organizations WHERE slug='${SLUG}'" | grep -q 1; then
  echo "error: org '${SLUG}' missing — run 'just dev-login' (or SLUG=… just dev-login) first" >&2
  exit 1
fi

echo "==> Postgres: enable public status + wipe prior fixtures"
pg <<SQL
UPDATE organizations SET name = 'Fixture Org' WHERE slug = '${SLUG}';

-- incident_updates + maintenance_window_components cascade via FK; wipe is
-- idempotent. Channels wiped by the same fixture name prefix. Branding and
-- component membership live on a status page (multi-page schema), recreated
-- below — dropping the page cascades its status_page_components.
DELETE FROM incidents
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
   AND target_id IN (
     SELECT id FROM targets
      WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
        AND tags @> ARRAY['seed-fixtures']);
-- Manually-declared fixtures carry no target, so the target-scoped wipe above
-- misses them; clear them by origin. incident_events cascade on delete.
DELETE FROM incidents
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
   AND origin = 'manual';
DELETE FROM targets
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
   AND tags @> ARRAY['seed-fixtures'];
-- target_regions cascade with their targets above. Regions + agents are owned
-- by the running stack and the dev-regions harness, never by the seed, so
-- nothing region-scoped is deleted here.
DELETE FROM notification_channels
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
   AND name LIKE 'Fixture %';
DELETE FROM status_pages
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
   AND slug IN ('fixtures', '${SLUG}');

-- Page slug must equal the subdomain label (= org slug): the public surface
-- resolves {slug}.{base_domain} against status_pages.slug directly.
INSERT INTO status_pages
  (org_id, slug, name, enabled, public_display_name, public_about, public_brand_color)
SELECT id, '${SLUG}', 'Fixture Status', true, 'Fixture Status',
       'Generated fixture data for UI stress-testing.', '#0ea5e9'
  FROM organizations WHERE slug = '${SLUG}';
SQL

echo "==> Postgres: pick first org member as owner FK for avatars"
OWNER_USER_ID=$(pg -tAc \
  "SELECT m.user_id::text FROM memberships m
     JOIN users u ON u.id = m.user_id AND u.deleted_at IS NULL
    WHERE m.org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
    ORDER BY m.created_at ASC LIMIT 1;" 2>/dev/null || true)
if [[ -z "$OWNER_USER_ID" ]]; then
  echo "    (no membership found — every monitor will render unowned)"
fi

echo "==> Postgres: team fixtures (teammate member + pending invite + second org for the nav picker)"
# Whole section needs an existing owner: the invite needs an inviter FK, the
# second org needs its owner, and seeding the teammate into a member-less org
# would make it the earliest membership — flipping the OWNER_USER_ID pick
# (ORDER BY created_at) to the teammate on the next run.
if [[ -n "$OWNER_USER_ID" ]]; then
pg <<SQL
-- Teammate: second member so /settings/team shows a real roster row with
-- role-toggle/remove actions. Create-if-absent keeps reruns idempotent.
INSERT INTO users (email, terms_version, privacy_version, email_verified_at, display_name)
SELECT 'teammate@fixture.test', 'v1', 'v1', now(), 'Fixture Teammate'
 WHERE NOT EXISTS (
   SELECT 1 FROM users WHERE email = 'teammate@fixture.test' AND deleted_at IS NULL);
INSERT INTO memberships (org_id, user_id, role)
SELECT o.id, u.id, 'member'
  FROM organizations o, users u
 WHERE o.slug = '${SLUG}' AND o.deleted_at IS NULL
   AND u.email = 'teammate@fixture.test' AND u.deleted_at IS NULL
ON CONFLICT DO NOTHING;

-- Pending invitation: renders the invitations table + revoke action.
-- Unredeemable because the dummy token_hash never parses as a PHC string,
-- so token verification always returns false — the prefix itself is a
-- perfectly valid lookup key.
DELETE FROM invitations
 WHERE org_id IN (SELECT id FROM organizations WHERE slug = '${SLUG}')
   AND email = 'pending@fixture.test';
INSERT INTO invitations
  (org_id, inviter_id, email, role, token_hash, token_prefix, expires_at)
SELECT id, '${OWNER_USER_ID}'::uuid, 'pending@fixture.test', 'member',
       'fixture-unredeemable', 'fixture-unredeem', now() + interval '7 days'
  FROM organizations WHERE slug = '${SLUG}' AND deleted_at IS NULL;

-- Second org owned by the dev user: makes the nav org picker render
-- (it only appears for multi-org sessions).
-- On the same account as the first org, so the two share one pool of caps
-- exactly as a real second org would.
INSERT INTO organizations (slug, name, account_id)
SELECT 'fixture-second', 'Fixture Second Org', a.id
  FROM accounts a
 WHERE a.owner_user_id = '${OWNER_USER_ID}'::uuid
   AND NOT EXISTS (
     SELECT 1 FROM organizations WHERE slug = 'fixture-second' AND deleted_at IS NULL);
INSERT INTO memberships (org_id, user_id, role)
SELECT o.id, '${OWNER_USER_ID}'::uuid, 'owner'
  FROM organizations o
 WHERE o.slug = 'fixture-second' AND o.deleted_at IS NULL
ON CONFLICT DO NOTHING;
SQL
else
  echo "    (skipping team fixtures — need an org member as inviter/owner)"
fi

echo "==> Postgres: insert 14 monitors (8 public/visible + 6 internal/varied-spec)"
# Most flags here exist to keep the seed counters from being clobbered by
# the live scheduler — a single real 'down' would flip Operational →
# PartialOutage. fix-search carries NULL group_name + NULL public_group
# so the Ungrouped path renders. group_name is operator-side, distinct
# from public_group, so the two surfaces can diverge.
pg <<SQL
WITH org AS (SELECT id FROM organizations WHERE slug='${SLUG}'),
     sp AS (SELECT id FROM status_pages
             WHERE org_id = (SELECT id FROM org) AND slug = '${SLUG}'),
     spec(name,is_enabled,public,pname,grp,so,gname,has_owner,spec) AS (VALUES
  -- Public / visible HTTP targets (group sort_order drives column layout).
  -- enabled=false on the Operational/Degraded/NoData ones — scheduler stray
  -- checks otherwise corrupt the pure-state seeded counters.
  ('fix-api',     false, true,  'API',             'Core Services',   0,  'API & Web',     true,
   '{"type":"http","url":"https://api.github.com/","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('fix-web',     false, true,  'Website',         'Core Services',   1,  'API & Web',     true,
   '{"type":"http","url":"https://www.google.com/","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('fix-cdn',     false, true,  'CDN',             'Core Services',   2,  'CDN',           true,
   '{"type":"http","url":"https://cdnjs.cloudflare.com/ajax/libs/jquery/3.7.1/jquery.min.js","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('fix-db',      true,  true,  'Database',        'Infrastructure',  0,  'Infrastructure',true,
   '{"type":"http","url":"https://www.cloudflare.com/cdn-cgi/trace","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('fix-auth',    false, true,  'Auth Service',    'Infrastructure',  1,  'Infrastructure',false,
   '{"type":"http","url":"https://login.microsoftonline.com/common/discovery/v2.0/keys","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('fix-email',   false, true,  'Email Delivery',  'Notifications',   0,  'Notifications', true,
   '{"type":"http","url":"https://en.wikipedia.org/wiki/Main_Page","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  -- Public component, NULL public_group AND NULL group_name → exercises
  -- the "Ungrouped" path on BOTH the public + Monitors pages.
  ('fix-search',  false, true,  'Search',          NULL,              0,  NULL,            false,
   '{"type":"http","url":"https://duckduckgo.com/","method":"HEAD","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"range","value":{"min":200,"max":399}},"headers":{},"verify_tls":true}'),
  -- Public component, enabled=false → renders disabled marker on operator
  -- page; still surfaces on the public page (filter is public_status only).
  ('fix-paused',  false, true,  'Beta Sandbox',    'Beta',            0,  'Beta',          true,
   '{"type":"http","url":"https://httpbin.org/status/200","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  -- Internal HTTP targets (legacy fixture rows; not on public page).
  ('fix-payment', true,  false, 'Payment Gateway', 'Internal',        0,  'Integrations',  true,
   '{"type":"http","url":"https://www.example.com/","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  ('fix-admin',   true,  false, 'Admin Portal',    'Internal',        1,  'Integrations',  false,
   '{"type":"http","url":"https://example.org/","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'),
  -- Non-HTTP check_spec variants — only the operator targets UI renders these.
  ('fix-tcp',     true,  false, 'Postgres Socket', 'Internal',        2,  'Infrastructure',true,
   '{"type":"tcp","host":"db.example.com","port":5432,"timeout":3000}'),
  ('fix-dns',     true,  false, 'DNS Apex',        'Internal',        3,  'Infrastructure',false,
   '{"type":"dns","domain":"example.com","record_type":"A","resolver":"1.1.1.1","expected_contains":"93.184","timeout":3000}'),
  ('fix-tls',     true,  false, 'TLS Cert',        'Internal',        4,  'Infrastructure',false,
   '{"type":"tls_cert","host":"example.com","port":443,"server_name":null,"warn_days":30,"critical_days":7,"timeout":5000}'),
  ('fix-domain',  true,  false, 'Domain Expiry',   'Internal',        5,  'Infrastructure',false,
   '{"type":"domain_expiry","domain":"example.com","warn_days":60,"critical_days":14,"timeout":10000}')
     ),
     ins AS (
       INSERT INTO targets
         (org_id, name, check_spec, interval_secs, enabled, tags, group_name, owner_user_id)
       SELECT org.id, spec.name, spec.spec::jsonb,
              CASE WHEN spec.spec::jsonb->>'type' IN ('tls_cert','domain_expiry') THEN 86400 ELSE 60 END,
              spec.is_enabled, ARRAY['seed-fixtures'], spec.gname,
              CASE WHEN spec.has_owner AND '${OWNER_USER_ID}' <> ''
                   THEN '${OWNER_USER_ID}'::uuid ELSE NULL END
       FROM org, spec
       RETURNING id, name
     )
-- Public monitors become components of the fixture status page; the rest stay
-- operator-only (and now still get incidents via the broadened writer).
INSERT INTO status_page_components
  (org_id, status_page_id, target_id, public_name, public_group, sort_order)
SELECT (SELECT id FROM org), (SELECT id FROM sp), ins.id, spec.pname, spec.grp, spec.so
FROM ins JOIN spec ON spec.name = ins.name
WHERE spec.public;

-- Demonstrate the managed-by badge: pretend the Integrations monitors are
-- authored by Terraform and one by a raw API token. UI-authored rows (the
-- default 'ui') stay chip-less.
UPDATE targets SET write_source = 'terraform'
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
   AND tags @> ARRAY['seed-fixtures'] AND name IN ('fix-payment','fix-admin');
UPDATE targets SET write_source = 'api'
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
   AND tags @> ARRAY['seed-fixtures'] AND name = 'fix-tcp';
SQL

ORG=$(pg -tAc "SELECT id FROM organizations WHERE slug='${SLUG}';")
: "${ORG:?org id query returned empty — preflight check should have caught this}"
read -r T_API T_WEB T_CDN T_DB T_AUTH T_EMAIL T_SEARCH T_PAUSED \
        T_PAY T_ADMIN T_TCP T_DNS T_TLS T_DOMAIN < <(pg -tAc \
  "SELECT string_agg(id::text,' ' ORDER BY array_position(
       ARRAY['fix-api','fix-web','fix-cdn','fix-db','fix-auth','fix-email',
             'fix-search','fix-paused',
             'fix-payment','fix-admin','fix-tcp','fix-dns','fix-tls','fix-domain'],
       name))
     FROM targets WHERE org_id='${ORG}' AND tags @> ARRAY['seed-fixtures'];")

# Guard against silent `read -r` misalignment: if any target name in the
# VALUES block above drifts from the ORDER BY array, string_agg sorts the
# unknown name LAST (array_position → NULL → NULLS LAST), shifting every
# positional assignment by one. Empty strings then become `''::uuid` cast
# errors many lines into the script. Validate up-front.
for _v in T_API T_WEB T_CDN T_DB T_AUTH T_EMAIL T_SEARCH T_PAUSED \
          T_PAY T_ADMIN T_TCP T_DNS T_TLS T_DOMAIN; do
  _id="${!_v}"
  if [[ ${#_id} -ne 36 ]]; then
    echo "error: ${_v}='${_id}' — expected 36-char UUID. Target VALUES list and ORDER BY array out of sync?" >&2
    exit 1
  fi
done
unset _v _id

echo "==> Postgres: read the real region topology + assign every monitor to it"
# Mirror the live create flow (default coverage = every enabled region) by
# assigning each fixture monitor to all regions that already exist — the home
# region plus any agents from `just dev-regions`. region_policy stays default
# (majority). The ClickHouse history below is written into these same regions.
mapfile -t SEED_REGIONS < <(pg -tAc "SELECT id FROM regions WHERE enabled ORDER BY id")
if (( ${#SEED_REGIONS[@]} == 0 )); then
  echo "error: no enabled regions — start the stack (and 'just dev-regions') first" >&2
  exit 1
fi
echo "    regions: ${SEED_REGIONS[*]}"
# ClickHouse array literal of the same regions for the per-region history writes.
CH_REGIONS=$(printf "'%s'," "${SEED_REGIONS[@]}"); CH_REGIONS="${CH_REGIONS%,}"

# Postgres text[] literals for the incident region breakdown: a partial outage
# marks one region down + the rest still up; a full outage marks all regions
# down. Single-region stacks get an empty "up" set (no breakdown rendered).
REGS_ALL="ARRAY[${CH_REGIONS}]::text[]"
REGS_DOWN_ONE="ARRAY['${SEED_REGIONS[0]}']::text[]"
if (( ${#SEED_REGIONS[@]} > 1 )); then
  _rest=$(printf "'%s'," "${SEED_REGIONS[@]:1}"); _rest="${_rest%,}"
  REGS_UP_REST="ARRAY[${_rest}]::text[]"
else
  REGS_UP_REST="ARRAY[]::text[]"
fi

pg <<SQL
INSERT INTO target_regions (target_id, region)
SELECT t.id, r.id FROM targets t CROSS JOIN regions r
 WHERE t.org_id = '${ORG}' AND t.tags @> ARRAY['seed-fixtures'] AND r.enabled
ON CONFLICT DO NOTHING;
SQL

# Public targets only — incidents on internal monitors wouldn't surface on
# the public status page anyway. Round-robin across 6 visible HTTP targets
# so the load is even. fix-search/fix-paused get their own targeted rows
# below; non-HTTP targets don't appear here.
VISIBLE_TARGETS=("$T_API" "$T_WEB" "$T_CDN" "$T_DB" "$T_AUTH" "$T_EMAIL")

echo "==> Postgres: 150 resolved incidents across 87d (all severities × all status starts × postmortem mix)"
# Layout: 150 rows × generate_series(1..150). 14h interval packs ~60 starts
# into the last 30 days (clears the page's 50-incident render cap → the
# "Older incidents →" link appears) while still spanning ~87 days so the
# archive paginates. Duration 5-185 min keeps the timeline visually diverse.
# Independent modulos decouple severity / status_at_start / title /
# postmortem-or-not so the badge combinations on the public page exercise
# every code path. Every ~5th incident gets a postmortem update appended
# AFTER the resolve so the Postmortem phase badge actually appears.
pg <<SQL
WITH s AS (
  SELECT n,
         CASE ((n-1) % 6)
           WHEN 0 THEN '${T_API}'::uuid
           WHEN 1 THEN '${T_WEB}'::uuid
           WHEN 2 THEN '${T_CDN}'::uuid
           WHEN 3 THEN '${T_DB}'::uuid
           WHEN 4 THEN '${T_AUTH}'::uuid
           ELSE        '${T_EMAIL}'::uuid
         END AS target_id,
         (now() - (n * interval '14 hour'))                          AS started_at,
         ((n % 180) + 5) * interval '1 minute'                       AS dur,
         -- Mod 3 / mod 3 / mod 4 are coprime to each other and to mod 6
         -- (target spread), so all 3×3×4 = 36 severity×status×err
         -- combinations appear across the 150-row run.
         (ARRAY['minor','major','critical'])[((n-1) % 3) + 1]        AS sev,
         (ARRAY['down','degraded','error'])[((n*2-1) % 3) + 1]       AS sas,
         -- Phase-specific reasons the probe now emits (TCP / DNS / TLS); the
         -- detail view maps "dns: …" to friendlier copy, the rest pass through.
         (ARRAY['connection refused','connect timeout','dns: domain not found','certificate expired','certificate not trusted','no response'])[((n-1) % 6) + 1] AS err,
         -- 12 distinct titles round-robined for archive variety.
         (ARRAY[
           'Elevated 5xx error rate',
           'Slow upstream response',
           'Timeout spike',
           'TCP connection failures',
           'Partial regional outage',
           'TLS handshake failures',
           'DNS resolution failures',
           'Database connection pool exhausted',
           'Memory pressure on edge nodes',
           'CDN cache miss surge',
           'Authentication subsystem degraded',
           'Email delivery queue backlog'
         ])[((n-1) % 12) + 1]                                        AS title,
         -- ~20% of incidents get a postmortem update (n % 5 == 0).
         (n % 5 = 0)                                                 AS with_postmortem
  FROM generate_series(1, 150) n
),
ins AS (
  INSERT INTO incidents
    (org_id, target_id, started_at, ended_at, severity, status_at_start,
     check_count, error_sample, public_title, public_description, duration_secs,
     state, visibility, origin, regions_down, regions_up)
  SELECT '${ORG}'::uuid, s.target_id, s.started_at, s.started_at + s.dur,
         s.sev, s.sas, (s.n % 20) + 5, s.err,
         s.title || ' (fixture #' || s.n || ')',
         'Auto-generated fixture incident #' || s.n
           || '. Severity ' || s.sev || ', start status ' || s.sas
           || ', duration ' || extract(epoch from s.dur)::int || 's.',
         extract(epoch from s.dur)::int,
         -- Closed, public, monitor-opened: operational state mirrors ended_at.
         'resolved', 'public', 'monitor',
         -- "Partial regional outage" rows carry a real breakdown (one region
         -- down, the rest up); the others read as a full all-region outage.
         CASE WHEN s.title = 'Partial regional outage' THEN ${REGS_DOWN_ONE} ELSE ${REGS_ALL} END,
         CASE WHEN s.title = 'Partial regional outage' THEN ${REGS_UP_REST} ELSE ARRAY[]::text[] END
  FROM s
  RETURNING id, started_at, ended_at
)
INSERT INTO incident_updates (org_id, incident_id, posted_at, phase, message, author)
SELECT '${ORG}', i.id, p.posted_at, p.phase::text, p.message, NULLIF('${OWNER_USER_ID}', '')
FROM ins i
JOIN s ON s.started_at = i.started_at
CROSS JOIN LATERAL (
  SELECT i.started_at                                              AS posted_at,
         'investigating'                                           AS phase,
         'Investigating elevated errors.'                          AS message
  UNION ALL SELECT i.started_at + (i.ended_at - i.started_at)*0.25,
         'identified',    'Root cause identified.'
  UNION ALL SELECT i.started_at + (i.ended_at - i.started_at)*0.75,
         'monitoring',    'Mitigation applied; monitoring.'
  UNION ALL SELECT i.ended_at,
         'resolved',      'Service fully restored.'
  UNION ALL SELECT i.ended_at + interval '2 hour',
         'postmortem',    'Postmortem published — see internal docs.'
         WHERE s.with_postmortem
) AS p(posted_at, phase, message);
SQL

echo "==> Postgres: 4 maintenance windows (1 active, 2 upcoming, 1 past) + bind active → fix-db"
# Binding the *active* window to fix-db flips that component's
# current_status to Maintenance (component_status() short-circuits on
# maintenance_active regardless of recent up/down counters).
pg <<SQL
DELETE FROM maintenance_windows
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
   AND title LIKE 'Fixture %';

WITH ins AS (
  INSERT INTO maintenance_windows (org_id, title, description, starts_at, ends_at)
  VALUES
    ('${ORG}'::uuid,
     'Fixture rolling database patch',
     'Read-only window while patching primary database.',
     now() - interval '30 minute', now() + interval '90 minute'),
    ('${ORG}'::uuid,
     'Fixture API gateway upgrade',
     'Scheduled upgrade. Brief 503s expected during failover.',
     now() + interval '2 day', now() + interval '2 day 1 hour'),
    ('${ORG}'::uuid,
     'Fixture CDN edge cutover',
     'Cutover to new CDN provider. No expected impact.',
     now() + interval '7 day', now() + interval '7 day 2 hour'),
    ('${ORG}'::uuid,
     'Fixture historical maintenance',
     'Past maintenance retained so the archive view has rows.',
     now() - interval '10 day', now() - interval '10 day' + interval '45 minute')
  RETURNING id, title
)
INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id)
SELECT '${ORG}'::uuid, ins.id, '${T_DB}'::uuid
FROM ins WHERE ins.title = 'Fixture rolling database patch';
SQL

echo "==> Postgres: 3 active incidents (Degraded / Partial / Major; all phases)"
# One OPEN incident per target — mirrors the writer's single-open-per-target
# invariant (insert_open guards NOT EXISTS open), so the public active banner
# never lists the same component twice. These drive the component states:
# status_at_start='degraded' → Degraded regardless of regions; otherwise a
# non-empty regions_up → PartialOutage, empty → MajorOutage. Seeded only on
# enabled=false public monitors: the live writer never checks disabled
# targets, so it can't auto-resolve these mid-demo. Excludes fix-db
# (Maintenance binding overrides incident impacts).
pg <<SQL
WITH s AS (
  SELECT target_id, phase, sev, sas, err,
         now() - (ord * interval '13 minute') AS started_at
  FROM (VALUES
    ('${T_WEB}'::uuid,  'investigating', 'major',    'degraded', 'rate-limited 503 (Retry-After: 30)', 1),
    ('${T_CDN}'::uuid,  'identified',    'major',    'down',     'connection refused',                 2),
    ('${T_AUTH}'::uuid, 'monitoring',    'critical', 'down',     'connect timeout',                    3)
  ) AS v(target_id, phase, sev, sas, err, ord)
),
ins AS (
  INSERT INTO incidents
    (org_id, target_id, started_at, ended_at, severity, status_at_start,
     check_count, error_sample, public_title, public_description, duration_secs,
     state, visibility, origin, acknowledged_at, acknowledged_by,
     regions_down, regions_up)
  SELECT '${ORG}'::uuid, s.target_id, s.started_at, NULL,
         s.sev, s.sas, 3, s.err,
         'Ongoing — ' || initcap(s.phase) || ' (' || s.phase || ' phase)',
         'Live fixture incident still in ' || s.phase || ' phase.',
         NULL,
         -- Past-investigating phases are acknowledged by the on-call owner;
         -- still-investigating ones are unacknowledged (triggered).
         CASE WHEN s.phase = 'investigating' THEN 'triggered' ELSE 'acknowledged' END,
         'public', 'monitor',
         CASE WHEN s.phase <> 'investigating'
              THEN s.started_at + interval '4 minute' ELSE NULL END,
         CASE WHEN s.phase <> 'investigating' AND '${OWNER_USER_ID}' <> ''
              THEN '${OWNER_USER_ID}'::uuid ELSE NULL END,
         -- fix-cdn is a live partial outage (one region down); the rest are
         -- full all-region outages.
         CASE WHEN s.target_id = '${T_CDN}'::uuid THEN ${REGS_DOWN_ONE} ELSE ${REGS_ALL} END,
         CASE WHEN s.target_id = '${T_CDN}'::uuid THEN ${REGS_UP_REST} ELSE ARRAY[]::text[] END
  FROM s
  RETURNING id, started_at
)
-- One investigating update on every ongoing incident; layer in identified
-- and monitoring rows when the target phase is past those.
INSERT INTO incident_updates (org_id, incident_id, posted_at, phase, message, author)
SELECT '${ORG}', i.id, p.posted_at, p.phase, p.message, NULLIF('${OWNER_USER_ID}', '')
FROM ins i
JOIN s ON s.started_at = i.started_at
CROSS JOIN LATERAL (
  SELECT i.started_at AS posted_at, 'investigating'::text AS phase,
         'Investigating elevated errors.' AS message
  UNION ALL
  SELECT i.started_at + interval '5 minute', 'identified',
         'Root cause identified — failover in progress.'
  WHERE s.phase IN ('identified','monitoring')
  UNION ALL
  SELECT i.started_at + interval '10 minute', 'monitoring',
         'Mitigation applied; monitoring recovery.'
  WHERE s.phase = 'monitoring'
) AS p;
SQL

echo "==> Postgres: published manual minor on fix-paused + midnight-boundary resolved on fix-api"
# Two targeted scenarios for the incident-driven page:
#  * fix-paused: a manually declared minor incident, published to the page —
#    severity maps to impact (minor → Degraded), so the component shows
#    Degraded while its incident card shows the Minor chip.
#  * fix-api: a resolved incident that ended EXACTLY at last UTC midnight —
#    yesterday's strip cell paints, today's stays clean (half-open boundary).
pg <<SQL
INSERT INTO incidents
  (org_id, target_id, started_at, ended_at, severity, status_at_start,
   check_count, title, public_title, public_description, duration_secs,
   state, visibility, origin, acknowledged_at, acknowledged_by)
VALUES
  ('${ORG}'::uuid, '${T_PAUSED}'::uuid, now() - interval '35 minute', NULL,
   'minor', 'down', 0,
   'Sandbox intermittently slow',
   'Sandbox intermittently slow',
   'Declared from a support report; sandbox responses are slow but succeeding.',
   NULL, 'acknowledged', 'public', 'manual', now() - interval '30 minute',
   CASE WHEN '${OWNER_USER_ID}' <> '' THEN '${OWNER_USER_ID}'::uuid ELSE NULL END);

INSERT INTO incident_updates (org_id, incident_id, posted_at, phase, message, author)
SELECT '${ORG}', i.id, i.started_at, 'investigating',
       'Looking into slow sandbox responses.', NULLIF('${OWNER_USER_ID}', '')
FROM incidents i
WHERE i.org_id='${ORG}' AND i.target_id='${T_PAUSED}'::uuid AND i.ended_at IS NULL;

INSERT INTO incidents
  (org_id, target_id, started_at, ended_at, severity, status_at_start,
   check_count, error_sample, public_title, public_description, duration_secs,
   state, visibility, origin, regions_down, regions_up)
VALUES
  ('${ORG}'::uuid, '${T_API}'::uuid,
   date_trunc('day', now()) - interval '3 hour',
   date_trunc('day', now()),
   'major', 'down', 8, 'no response',
   'API outage resolved at midnight',
   'Ended exactly on the UTC day boundary — paints yesterday, not today.',
   10800, 'resolved', 'public', 'monitor', ${REGS_ALL}, ARRAY[]::text[]);
SQL

echo "==> Postgres: operational-layer incidents (internal-only + manually declared) with activity timelines"
# These exercise the operator incidents console specifically:
#   * internal incidents on non-public monitors (visibility='internal') — they
#     appear in the console but NEVER on the public status page, proving the
#     writer can open incidents for any monitor without leaking it publicly.
#   * a manually declared incident (origin='manual', no monitor) acknowledged
#     by the on-call owner.
# Each gets incident_events so the detail page's Activity timeline is populated.
pg <<SQL
WITH ins AS (
  INSERT INTO incidents
    (org_id, target_id, started_at, ended_at, severity, status_at_start,
     check_count, error_sample, title, duration_secs, state, visibility, origin)
  VALUES
    ('${ORG}'::uuid, '${T_PAY}'::uuid, now() - interval '40 minute', NULL,
     'critical', 'down', 4, 'connection refused',
     'Payment gateway unreachable', NULL, 'triggered', 'internal', 'monitor'),
    ('${ORG}'::uuid, '${T_ADMIN}'::uuid, now() - interval '3 hour', now() - interval '2 hour',
     'major', 'error', 6, 'http 500',
     'Admin portal 5xx spike', 3600, 'resolved', 'internal', 'monitor')
  RETURNING id, started_at, state
)
INSERT INTO incident_events (org_id, incident_id, occurred_at, kind, actor_type, actor_id, message)
SELECT '${ORG}'::uuid, i.id, e.occurred_at, e.kind, e.actor_type,
       CASE WHEN e.actor_type = 'user' AND '${OWNER_USER_ID}' <> ''
            THEN '${OWNER_USER_ID}'::uuid ELSE NULL END,
       e.message
FROM ins i
CROSS JOIN LATERAL (
  SELECT i.started_at AS occurred_at, 'triggered'::text AS kind,
         'system'::text AS actor_type, NULL::text AS message
  UNION ALL SELECT i.started_at + interval '6 minute', 'acknowledged', 'user', 'Taking a look.'
    WHERE i.state <> 'triggered'
  UNION ALL SELECT i.started_at + interval '55 minute', 'resolved', 'user', 'Rolled back the bad deploy.'
    WHERE i.state = 'resolved'
) AS e(occurred_at, kind, actor_type, message);

WITH ins AS (
  INSERT INTO incidents
    (org_id, target_id, started_at, ended_at, severity, status_at_start,
     check_count, title, duration_secs, state, visibility, origin,
     acknowledged_at, acknowledged_by)
  VALUES
    ('${ORG}'::uuid, NULL, now() - interval '20 minute', NULL,
     'major', 'down', 0, 'Customer-reported checkout failures', NULL,
     'acknowledged', 'internal', 'manual', now() - interval '15 minute',
     CASE WHEN '${OWNER_USER_ID}' <> '' THEN '${OWNER_USER_ID}'::uuid ELSE NULL END)
  RETURNING id, started_at
)
INSERT INTO incident_events (org_id, incident_id, occurred_at, kind, actor_type, actor_id, message)
SELECT '${ORG}'::uuid, i.id, e.occurred_at, e.kind, e.actor_type,
       CASE WHEN '${OWNER_USER_ID}' <> '' THEN '${OWNER_USER_ID}'::uuid ELSE NULL END,
       e.message
FROM ins i
CROSS JOIN LATERAL (
  SELECT i.started_at AS occurred_at, 'triggered'::text AS kind,
         'user'::text AS actor_type, 'Declared from support ticket #4821.'::text AS message
  UNION ALL SELECT i.started_at + interval '5 minute', 'acknowledged', 'user', 'On call investigating.'
  UNION ALL SELECT i.started_at + interval '8 minute', 'note', 'user',
         'Correlated with the payment gateway incident.'
) AS e(occurred_at, kind, actor_type, message);
SQL

echo "==> Postgres: who acknowledged each acknowledged incident"
# The owner took every acknowledged fixture incident; list them the way the
# acknowledge action does. No fixture incident was reopened, so episode 0.
pg <<SQL
INSERT INTO incident_acknowledgements
  (org_id, incident_id, episode, actor_type, actor_id, anonymous, acknowledged_at)
SELECT org_id, id, 0, 'user', acknowledged_by, false, acknowledged_at
FROM incidents
WHERE org_id = '${ORG}'::uuid AND acknowledged_at IS NOT NULL AND acknowledged_by IS NOT NULL
ON CONFLICT DO NOTHING;
SQL

echo "==> Postgres: 15 notification channels (one per transport) and alert bindings"
# Channel kinds match ChannelConfig — one per variant. The BYO Telegram row is
# disabled so the operator UI renders both the enabled and disabled states;
# the linked telegram_app row carries the platform-disable note.
# Bindings are pure delivery targets ({channel_id}); the firing policy
# (alert_confirmations, notify_recovery) lives on the monitor and is set below.
pg <<SQL
INSERT INTO notification_channels (org_id, name, kind, config, external_ref, enabled, disabled_reason) VALUES
  ('${ORG}'::uuid, 'Fixture Slack',    'slack',
   '{"type":"slack","webhook_url":"https://hooks.slack.com/services/T0000/B0000/XXXXXXXXXXXXXXXXXXXXXXXX"}'::jsonb,
   NULL, true, NULL),
  ('${ORG}'::uuid, 'Fixture Slack App', 'slack_app',
   '{"type":"slack_app","webhook_url":"https://hooks.slack.com/services/T0000/B0001/XXXXXXXXXXXXXXXXXXXXXXXX","channel":"#fixture-ops","channel_id":"C0FIXTURE01","team_id":"T0FIXTURE01"}'::jsonb,
   'C0FIXTURE01', true, NULL),
  ('${ORG}'::uuid, 'Fixture Webhook',  'webhook',
   '{"type":"webhook","url":"https://example.com/hook","headers":{"X-Fixture":"1"}}'::jsonb,
   NULL, true, NULL),
  ('${ORG}'::uuid, 'Fixture Telegram', 'telegram',
   '{"type":"telegram","bot_token":"1234567890:AAH-fixture-bot-token","chat_id":"-1001234567890"}'::jsonb,
   NULL, false, NULL),
  ('${ORG}'::uuid, 'Fixture Tg Linked', 'telegram_app',
   '{"type":"telegram_app","chat_id":"-1009876543210","chat_title":"Fixture Ops"}'::jsonb,
   '-1009876543210', false, 'unlinked from the Telegram side'),
  ('${ORG}'::uuid, 'Fixture WhatsApp', 'whatsapp',
   '{"type":"whatsapp","access_token":"EAAG-fixture-token","phone_number_id":"106540352242922","to":"15551234567","template_name":"uptime_alert"}'::jsonb,
   NULL, true, NULL),
  ('${ORG}'::uuid, 'Fixture Discord', 'discord',
   '{"type":"discord","webhook_url":"https://discord.com/api/webhooks/000000000000000000/fixture-discord-token"}'::jsonb,
   NULL, true, NULL),
  ('${ORG}'::uuid, 'Fixture Discord App', 'discord_app',
   '{"type":"discord_app","webhook_url":"https://discord.com/api/webhooks/100000000000000001/fixture-discord-app-token","webhook_id":"100000000000000001","mention":"&100000000000000002"}'::jsonb,
   '100000000000000001', true, NULL),
  ('${ORG}'::uuid, 'Fixture Teams', 'msteams',
   '{"type":"msteams","webhook_url":"https://prod-00.westus.logic.azure.com/workflows/fixture/triggers/manual/paths/invoke"}'::jsonb,
   NULL, true, NULL),
  ('${ORG}'::uuid, 'Fixture GChat', 'google_chat',
   '{"type":"google_chat","webhook_url":"https://chat.googleapis.com/v1/spaces/AAAA0000/messages?key=fixture-key&token=fixture-token"}'::jsonb,
   NULL, true, NULL),
  ('${ORG}'::uuid, 'Fixture Email', 'email',
   '{"type":"email","to":"oncall@example.com"}'::jsonb,
   'oncall@example.com', true, NULL),
  ('${ORG}'::uuid, 'Fixture WA Linked', 'whatsapp_app',
   '{"type":"whatsapp_app","phone":"15551234567","profile_name":"Fixture Jane"}'::jsonb,
   '15551234567', true, NULL),
  ('${ORG}'::uuid, 'Fixture PagerDuty', 'pagerduty',
   '{"type":"pagerduty","routing_key":"fixturefixturefixturefixture0000"}'::jsonb,
   NULL, true, NULL),
  ('${ORG}'::uuid, 'Fixture Ntfy', 'ntfy',
   '{"type":"ntfy","server_url":"https://ntfy.sh","topic":"fixture-alerts","access_token":"tk_fixturetoken"}'::jsonb,
   NULL, true, NULL),
  ('${ORG}'::uuid, 'Fixture Pushover', 'pushover',
   '{"type":"pushover","token":"azGDORePK8gMaC0QOYAMyEEuzJnyUi","user":"uQiRzpo4DXghDmr9QzzfQu27cmVRsG","device":"fixture-phone","emergency":true}'::jsonb,
   NULL, true, NULL);

-- The email fixture renders the unverified chip; engine deliveries to it
-- record failures, which is the state worth eyeballing.


-- Managed-by badge on the channels list too: webhook as Terraform, Slack as
-- a raw API token; Telegram stays UI-authored (no chip).
UPDATE notification_channels SET write_source = 'terraform'
 WHERE org_id='${ORG}'::uuid AND name = 'Fixture Webhook';
UPDATE notification_channels SET write_source = 'api'
 WHERE org_id='${ORG}'::uuid AND name = 'Fixture Slack';

-- Three monitors, each a different notification shape — and a different reminder
-- cadence so the form's renotify dropdown renders hourly / off / 15-minute:
--   fix-api  : both enabled channels, 3 confirmations, recovery on,  remind hourly
--   fix-db   : Slack only,            5 confirmations, recovery off,  reminders off
--   fix-auth : Slack + Telegram,      2 confirmations, recovery on,   remind every 15m
-- COALESCE guards against a future rename / typo in the WHERE clause:
-- jsonb_agg over zero rows returns NULL, which violates alerts NOT NULL.
UPDATE targets SET
  alerts = COALESCE((
    SELECT jsonb_agg(jsonb_build_object('channel_id', id))
      FROM notification_channels
     WHERE org_id='${ORG}'::uuid AND name IN ('Fixture Slack','Fixture Webhook')
  ), '[]'::jsonb),
  alert_confirmations = 3, notify_recovery = true, renotify_interval_secs = 3600
WHERE id='${T_API}'::uuid;

UPDATE targets SET
  alerts = COALESCE((
    SELECT jsonb_agg(jsonb_build_object('channel_id', id))
      FROM notification_channels
     WHERE org_id='${ORG}'::uuid AND name = 'Fixture Slack'
  ), '[]'::jsonb),
  alert_confirmations = 5, notify_recovery = false, renotify_interval_secs = 0
WHERE id='${T_DB}'::uuid;

UPDATE targets SET
  alerts = COALESCE((
    SELECT jsonb_agg(jsonb_build_object('channel_id', id))
      FROM notification_channels
     WHERE org_id='${ORG}'::uuid AND name IN ('Fixture Slack','Fixture Telegram')
  ), '[]'::jsonb),
  alert_confirmations = 2, notify_recovery = true, renotify_interval_secs = 900
WHERE id='${T_AUTH}'::uuid;

-- fix-tcp pages the Pushover channel (emergency priority on). It stays down in
-- the fixture, so its incident remains triggered and acknowledgeable — the
-- repeat-until-ack delivery + timeline are eyeball-able. Reminders off so the
-- renotify sweep doesn't re-page a fixture box.
UPDATE targets SET
  alerts = COALESCE((
    SELECT jsonb_agg(jsonb_build_object('channel_id', id))
      FROM notification_channels
     WHERE org_id='${ORG}'::uuid AND name = 'Fixture Pushover'
  ), '[]'::jsonb),
  alert_confirmations = 2, notify_recovery = true, renotify_interval_secs = 0
WHERE id='${T_TCP}'::uuid;
SQL

echo "==> Postgres: 1 adversarial-title incident (XSS / day-popover JSON-escape smoke)"
# Title carries <, >, &, ", </script>, <!--, control bytes — covers the
# escape path that emits the inline #day-strip-data JSON blob safe-by-default.
pg <<SQL
INSERT INTO incidents
  (org_id, target_id, started_at, ended_at, severity, status_at_start,
   check_count, error_sample, public_title, public_description, duration_secs,
   state, visibility, origin)
VALUES
  ('${ORG}'::uuid, '${T_SEARCH}'::uuid,
   now() - interval '36 hour', now() - interval '35 hour',
   'minor', 'degraded', 7, 'parse error',
   \$ADV\$Adversarial title </script><!-- & "double" — fixture smoke\$ADV\$,
   \$ADV\$Tests JSON escaping for the day_strip popover blob. Contains < > & " ' </script> <!-- and a unicode em-dash —.\$ADV\$,
   3600, 'resolved', 'public', 'monitor');

INSERT INTO incident_updates (org_id, incident_id, posted_at, phase, message, author)
SELECT '${ORG}'::uuid, i.id, p.posted_at, p.phase, p.message, NULLIF('${OWNER_USER_ID}', '')
FROM (SELECT id, started_at, ended_at FROM incidents
       WHERE org_id='${ORG}'::uuid AND target_id='${T_SEARCH}'::uuid
         AND public_title LIKE 'Adversarial title%') i
CROSS JOIN LATERAL (
  SELECT i.started_at AS posted_at, 'investigating'::text AS phase,
         'Investigating <script>alert(1)</script> — content escaped client-side.' AS message
  UNION ALL SELECT i.ended_at, 'resolved', 'Resolved & safe.'
) p;
SQL

# Mark the seeded 'triggered' incidents as already-paged. Two background sweeps
# act on a triggered incident otherwise: reconcile re-pages one with no
# notification row at all, and renotify re-pages one whose last page is older
# than the monitor's reminder interval. Synthesize one delivered 'opened'
# notification each, stamped now() (not started_at) so the reminder is a full
# interval out and neither sweep churns on boot. Attributed to the webhook
# channel so the incident's Delivery section shows a channel name. Cascades on
# incident delete, so re-runs stay clean.
pg <<SQL
INSERT INTO incident_notifications (org_id, incident_id, channel_id, transport, reason, status, attempt, sent_at)
SELECT i.org_id, i.id,
       (SELECT id FROM notification_channels WHERE org_id = i.org_id AND kind = 'webhook' LIMIT 1),
       'webhook', 'opened', 'sent', 1, now()
FROM incidents i
WHERE i.org_id = '${ORG}'::uuid AND i.state = 'triggered';

-- Delivery-log coverage: give one open incident a retrying page and a
-- dead-lettered one too, so the incident Delivery section renders all of
-- sent / retrying / dead-letter at once. next_attempt_at sits an hour out (or
-- NULL for the dead-letter) so the retry sweep stays quiet on a fixture box.
INSERT INTO incident_notifications
  (org_id, incident_id, channel_id, transport, reason, status, attempt, error, next_attempt_at)
SELECT i.org_id, i.id,
       (SELECT id FROM notification_channels WHERE org_id = i.org_id AND kind = 'slack' LIMIT 1),
       'slack', 'opened', v.status, v.attempt, v.error, v.next_attempt_at
FROM incidents i
CROSS JOIN (VALUES
  ('failed'::text, 3, 'connection timed out', now() + interval '1 hour'),
  ('failed'::text, 5, 'connection refused',   NULL::timestamptz)
) AS v(status, attempt, error, next_attempt_at)
WHERE i.org_id = '${ORG}'::uuid AND i.target_id = '${T_PAY}'::uuid AND i.state = 'triggered';

-- Pushover emergency (priority 2, repeat-until-acknowledged): one page still
-- awaiting acknowledgement (receipt set, acked_at NULL) and one already
-- acknowledged (acked_at set). Renders both states in the fix-tcp incident's
-- Delivery section. sent_at is minutes old so neither looks freshly fired.
INSERT INTO incident_notifications
  (org_id, incident_id, channel_id, escalation_level, transport, reason, status, attempt, sent_at, provider_receipt, acked_at)
SELECT i.org_id, i.id,
       (SELECT id FROM notification_channels WHERE org_id = i.org_id AND kind = 'pushover' LIMIT 1),
       v.level, 'pushover', 'opened', 'sent', 1, v.sent_at, v.receipt, v.acked_at
FROM incidents i
CROSS JOIN (VALUES
  (0, now() - interval '8 minutes', 'rcpt-fixture-pending'::text, NULL::timestamptz),
  (1, now() - interval '6 minutes', 'rcpt-fixture-acked'::text,   now() - interval '4 minutes')
) AS v(level, sent_at, receipt, acked_at)
WHERE i.org_id = '${ORG}'::uuid AND i.target_id = '${T_TCP}'::uuid AND i.state = 'triggered';

-- The note the poll sweep writes when Pushover reports the page acknowledged.
INSERT INTO incident_events (org_id, incident_id, occurred_at, kind, actor_type, message)
SELECT i.org_id, i.id, now() - interval '4 minutes', 'note', 'system',
       'emergency page acknowledged in Pushover'
FROM incidents i
WHERE i.org_id = '${ORG}'::uuid AND i.target_id = '${T_TCP}'::uuid AND i.state = 'triggered';
SQL

if [ "$RESET_CH" = "1" ]; then
  echo "==> ClickHouse: purging existing rows for fixture targets"
  # Every tid below was length-validated post-`read -r`; an empty tid here
  # would build `toUUID('')` and either abort the loop mid-purge (modern CH)
  # or — on older CH — purge the zero-UUID partition.
  for tid in "${VISIBLE_TARGETS[@]}" "$T_SEARCH" "$T_PAUSED" "$T_PAY" "$T_ADMIN" \
             "$T_TCP" "$T_DNS" "$T_TLS" "$T_DOMAIN"; do
    ch -q "ALTER TABLE monitor.check_results DELETE WHERE target_id=toUUID('${tid}') SETTINGS mutations_sync=1"
  done
fi

echo "==> ClickHouse: 90d baseline history for visible monitors (per-target divergent shape)"
# Mostly-up baseline + outage spikes + a few degraded days. Every row is
# shifted 6h into the past so day_index=0 rows can't leak into the
# last-5-min classifier window. Shape diverges per target via cityHash64(tid):
#   * outage count    : 4..15  (uptime% then ranges ~83%..96% per target)
#   * outage day-set  : prime-stride walk so two targets rarely share a day
#   * degraded count  : 1..5   (rendered as PartialOutage cells — the day-
#                              strip MV doesn't preserve down/degraded yet,
#                              so degraded *days* collapse into the outage
#                              count; component-level Degraded state is
#                              still surfaced via fix-web's last-5-min mix)
#   * latency band    : 80..200 ms baseline so per-target sparkline differs
# Plus an explicit 87-89d "ancient outage" cluster on the first 3 visible
# monitors so the leftmost day-strip cells have a guaranteed downtime marker
# (otherwise the prime-stride walk rarely lands deep into the 90d window).
# fix-email is handled separately so we can punch a NoData gap into it.
for tid in "$T_API" "$T_WEB" "$T_CDN" "$T_DB" "$T_AUTH" "$T_SEARCH"; do
  ch -mn <<SQL
INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${tid}'),r,
       now() - toIntervalHour(6) - toIntervalDay(number) - toIntervalMinute(number % 47),
       'up',
       80 + (cityHash64('${tid}') % 120) + (number % 35),
       200
FROM numbers(90) ARRAY JOIN [${CH_REGIONS}] AS r;

-- Down spikes pinned to days 0..59 so they never collide with degraded days
-- (60..86) — keeps the day_state classifier from masking Degraded under
-- PartialOutage when both types land on the same calendar day.
INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code,error)
SELECT toUUID('${ORG}'),toUUID('${tid}'),r,
       now() - toIntervalHour(6)
             - toIntervalDay((number * 7 + (cityHash64('${tid}') % 13)) % 60)
             - toIntervalMinute(((number * 11 + cityHash64('${tid}')) % 60)),
       'down', 0, 503, 'connection refused'
FROM numbers(20) ARRAY JOIN [${CH_REGIONS}] AS r
WHERE number < (4 + (cityHash64('${tid}') % 12));

INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${tid}'),r,
       now() - toIntervalHour(6)
             - toIntervalDay(60 + (number * 3 + (cityHash64('${tid}') % 7)) % 27)
             - toIntervalMinute(((number * 23 + cityHash64('${tid}')) % 60)),
       'degraded',
       600 + (cityHash64('${tid}') % 400) + (number * 40),
       200
FROM numbers(8) ARRAY JOIN [${CH_REGIONS}] AS r
WHERE number < (1 + (bitShiftRight(cityHash64('${tid}'), 8) % 5));
SQL
done

echo "==> ClickHouse: ancient outage clusters (day 87 / 88 / 89) for old-history smoke"
# Forces a visible red cell on the LEFT edge of the day-strip on three
# components — exercises rendering of the very oldest history bucket.
ch -mn <<SQL
INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code,error)
SELECT toUUID('${ORG}'),toUUID('${T_API}'),r,
       now() - toIntervalDay(89) - toIntervalMinute(15 + number * 3),
       'down', 0, 503, 'historical outage (89d ago)'
FROM numbers(6) ARRAY JOIN [${CH_REGIONS}] AS r;

INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code,error)
SELECT toUUID('${ORG}'),toUUID('${T_WEB}'),r,
       now() - toIntervalDay(88) - toIntervalMinute(45 + number * 4),
       'down', 0, 504, 'historical outage (88d ago)'
FROM numbers(5) ARRAY JOIN [${CH_REGIONS}] AS r;

INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code,error)
SELECT toUUID('${ORG}'),toUUID('${T_CDN}'),r,
       now() - toIntervalDay(87) - toIntervalMinute(120 + number * 5),
       'down', 0, 502, 'historical outage (87d ago)'
FROM numbers(4) ARRAY JOIN [${CH_REGIONS}] AS r;
SQL

echo "==> ClickHouse: fix-email 90d history with a 6-day NoData gap"
# Skips day_index 5..10 entirely so those day-strip cells render as
# DayState::NoData (grey, "no data" tooltip) — the only path that produces
# this state. Same 6h shift as the baseline keeps day-0 rows out of the
# last-5-min window.
ch -mn <<SQL
INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${T_EMAIL}'),r,
       now() - toIntervalHour(6) - toIntervalDay(number) - toIntervalMinute(number % 47),
       'up',
       110 + (number % 25),
       200
FROM numbers(90) ARRAY JOIN [${CH_REGIONS}] AS r
WHERE number < 5 OR number > 10;
SQL

echo "==> ClickHouse: raw check rows aligned with every public incident window"
# Mirror real engine output: an incident only ever exists because raw failing
# checks confirmed it, so backfill one bad row per minute per down region for
# each public incident's window (and up rows for the regions that stayed up).
# Without this the public strip paints a day the operator's raw charts call
# empty — a mismatch the real pipeline cannot produce. regions arrays come
# straight from the incident rows; manual incidents (NULL regions) add nothing.
ch -mn <<SQL
INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code,error)
SELECT toUUID(org_id), toUUID(target_id), r,
       started + toIntervalSecond(60 * n),
       multiIf(sas = 'degraded', 'degraded', sas = 'error', 'error', 'down'),
       if(sas = 'degraded', 900, 5000),
       if(sas = 'degraded', 200, NULL),
       err
FROM (
  SELECT org_id, target_id, regions_down, started_at AS started,
         status_at_start AS sas, coalesce(error_sample, 'connection refused') AS err,
         arrayJoin(range(toUInt32(greatest(1,
           dateDiff('second', started_at, coalesce(ended_at, now())) / 60)))) AS n
  FROM postgresql('${PG_CONTAINER}:5432','monitor','incidents','monitor','monitor')
  WHERE org_id = '${ORG}' AND visibility = 'public'
    AND started_at >= now() - INTERVAL 90 DAY
    AND regions_down IS NOT NULL AND length(regions_down) > 0
)
ARRAY JOIN regions_down AS r;

INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code)
SELECT toUUID(org_id), toUUID(target_id), r,
       started + toIntervalSecond(60 * n),
       'up', 120 + (n % 40), 200
FROM (
  SELECT org_id, target_id, regions_up, started_at AS started,
         arrayJoin(range(toUInt32(greatest(1,
           dateDiff('second', started_at, coalesce(ended_at, now())) / 60)))) AS n
  FROM postgresql('${PG_CONTAINER}:5432','monitor','incidents','monitor','monitor')
  WHERE org_id = '${ORG}' AND visibility = 'public'
    AND started_at >= now() - INTERVAL 90 DAY
    AND regions_up IS NOT NULL AND length(regions_up) > 0
)
ARRAY JOIN regions_up AS r;
SQL

echo "==> ClickHouse: last-5-min raw writes — operator drawer only, page state ignores these"
# Component state now derives from confirmed incidents, NOT these rows — they
# exist so the operator detail drawer, region breakdown, and latency charts
# have realistic recent data, and so the eyeball test can prove the page
# ignores raw failures. 30 rows at 4s spacing per region. Mix per target:
#   fix-api    2 fresh 'error' blips in ONE region, rest up — the page and
#              strip MUST stay green (no confirmed incident); the region
#              drawer shows the raw failures. This is the deploy-blip case.
#   fix-web    100% degraded — matches its open degraded incident.
#   fix-cdn    ~30% down     — matches its open partial incident.
#   fix-auth   ~70% down     — matches its open major incident.
#   fix-search 100% up.
# fix-db skipped (Maintenance binding). fix-email skipped (NoData gap demo).
ch -mn <<SQL
INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code,error)
SELECT toUUID('${ORG}'),toUUID('${T_API}'),r,
       now() - toIntervalSecond(number*4),
       if(number < 2 AND r = '${SEED_REGIONS[0]}', 'error', 'up'),
       if(number < 2 AND r = '${SEED_REGIONS[0]}', 5000, 90 + (number % 25)),
       if(number < 2 AND r = '${SEED_REGIONS[0]}', NULL, 200),
       if(number < 2 AND r = '${SEED_REGIONS[0]}', 'no response', NULL)
FROM numbers(30) ARRAY JOIN [${CH_REGIONS}] AS r;

INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${T_WEB}'),r,
       now() - toIntervalSecond(number*4),
       'degraded', 600 + (number * 40), 200
FROM numbers(30) ARRAY JOIN [${CH_REGIONS}] AS r;

# Down rows model a TLS-phase failure: DNS + TCP completed (timings present),
# the handshake was rejected (no tls_ms, no response_code) — exercises the
# detail view's expandable partial-timing breakdown on a connect failure.
INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code,error,dns_ms,connect_ms)
SELECT toUUID('${ORG}'),toUUID('${T_CDN}'),r,
       now() - toIntervalSecond(number*4),
       if(number % 10 < 3, 'down', 'up'),
       if(number % 10 < 3, 0, 80 + (number % 30)),
       if(number % 10 < 3, NULL, 200),
       if(number % 10 < 3, 'certificate expired', NULL),
       if(number % 10 < 3, toUInt16(11 + (number % 5)), NULL),
       if(number % 10 < 3, toUInt16(40 + (number % 8)), NULL)
FROM numbers(30) ARRAY JOIN [${CH_REGIONS}] AS r;

# Down rows model a TCP-phase failure: DNS resolved (dns_ms present) but the
# connection was refused — connect never completed, so connect_ms stays NULL,
# matching ConnectError::Connect's partial timings.
INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code,error,dns_ms)
SELECT toUUID('${ORG}'),toUUID('${T_AUTH}'),r,
       now() - toIntervalSecond(number*4),
       if(number % 10 < 7, 'down', 'up'),
       if(number % 10 < 7, 0, 95 + (number % 20)),
       if(number % 10 < 7, NULL, 200),
       if(number % 10 < 7, 'connection refused', NULL),
       if(number % 10 < 7, toUInt16(9 + (number % 4)), NULL)
FROM numbers(30) ARRAY JOIN [${CH_REGIONS}] AS r;

INSERT INTO monitor.check_results (org_id,target_id,region,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${T_SEARCH}'),r,
       now() - toIntervalSecond(number*4),
       'up', 70 + (number % 20), 200
FROM numbers(30) ARRAY JOIN [${CH_REGIONS}] AS r;
SQL

echo
echo "=========================================================================="
echo "  Post-seed verification"
echo "=========================================================================="

# ── Postgres row counts ──────────────────────────────────────────────────────
echo
echo "## Postgres counts"
pg -tA <<SQL | column -t -s '|'
SELECT 'targets (fixture)'   AS what, count(*)::text AS n FROM targets
  WHERE org_id='${ORG}' AND tags @> ARRAY['seed-fixtures']
UNION ALL
SELECT 'incidents',            count(*)::text FROM incidents WHERE org_id='${ORG}'
UNION ALL
SELECT '  └ resolved',         count(*)::text FROM incidents WHERE org_id='${ORG}' AND ended_at IS NOT NULL
UNION ALL
SELECT '  └ active',           count(*)::text FROM incidents WHERE org_id='${ORG}' AND ended_at IS NULL
UNION ALL
SELECT 'incident_updates',     count(*)::text FROM incident_updates WHERE org_id='${ORG}'
UNION ALL
SELECT 'maintenance_windows',  count(*)::text FROM maintenance_windows WHERE org_id='${ORG}' AND title LIKE 'Fixture %'
UNION ALL
SELECT '  └ active now',       count(*)::text FROM maintenance_windows
  WHERE org_id='${ORG}' AND title LIKE 'Fixture %' AND starts_at <= now() AND ends_at > now()
UNION ALL
SELECT '  └ component bindings', count(*)::text FROM maintenance_window_components WHERE org_id='${ORG}'
UNION ALL
SELECT 'notification_channels', count(*)::text FROM notification_channels WHERE org_id='${ORG}' AND name LIKE 'Fixture %'
UNION ALL
SELECT 'targets w/ alerts',    count(*)::text FROM targets
  WHERE org_id='${ORG}' AND tags @> ARRAY['seed-fixtures'] AND jsonb_array_length(alerts) > 0;
SQL

# ── Regions, assignments, agents (real topology this seed wrote onto) ─────────
echo
echo "## Regions & agents"
pg -tA <<SQL | column -t -s '|'
SELECT 'enabled regions'      AS what, count(*)::text AS n FROM regions WHERE enabled
UNION ALL
SELECT 'agents',              count(*)::text FROM agents
UNION ALL
SELECT '  └ stale (>90s)',    count(*)::text FROM agents
  WHERE enabled AND coalesce(last_seen_at, created_at) < now() - interval '90 seconds'
UNION ALL
SELECT 'fixture region assignments', count(*)::text FROM target_regions tr
  JOIN targets t ON t.id = tr.target_id
 WHERE t.org_id='${ORG}' AND t.tags @> ARRAY['seed-fixtures'];
SQL

# ── Expected/actual component-state matrix (confirmed-incident semantics) ────
# States derive from open PUBLIC incidents + the active maintenance binding —
# the SQL below mirrors PaintWindowRow::impact() + component_status():
# maintenance wins; manual incidents map severity minor/major/critical →
# Degraded/Partial/Major; auto incidents map degraded → Degraded, else
# regions_up non-empty → Partial, empty → Major; no open incident → Operational.
echo
echo "## Component states (from open public incidents + maintenance)"
pg_states=$(pg -tA -F $'\t' <<SQL
SELECT spc.public_name,
       CASE
         WHEN maint.target_id IS NOT NULL THEN 'Maintenance'
         WHEN agg.worst = 3 THEN 'MajorOutage'
         WHEN agg.worst = 2 THEN 'PartialOutage'
         WHEN agg.worst = 1 THEN 'Degraded'
         ELSE 'Operational'
       END AS state
FROM status_page_components spc
LEFT JOIN (
  SELECT i.target_id,
         max(CASE
           WHEN i.origin = 'manual' THEN
             CASE i.severity WHEN 'minor' THEN 1 WHEN 'major' THEN 2 ELSE 3 END
           WHEN i.status_at_start = 'degraded' THEN 1
           WHEN coalesce(array_length(i.regions_up, 1), 0) > 0 THEN 2
           ELSE 3
         END) AS worst
  FROM incidents i
  WHERE i.org_id = '${ORG}' AND i.ended_at IS NULL AND i.visibility = 'public'
  GROUP BY i.target_id
) agg ON agg.target_id = spc.target_id
LEFT JOIN (
  SELECT DISTINCT mwc.target_id
  FROM maintenance_window_components mwc
  JOIN maintenance_windows mw ON mw.id = mwc.maintenance_id
  WHERE mw.org_id = '${ORG}' AND mw.starts_at <= now() AND mw.ends_at > now()
) maint ON maint.target_id = spc.target_id
WHERE spc.org_id = '${ORG}'
ORDER BY coalesce(spc.public_group, 'zzz'), spc.sort_order;
SQL
)

# Expected state matrix — keep in lockstep with the header comment above.
declare -A EXPECT=(
  [API]=Operational
  [Website]=Degraded
  [CDN]=PartialOutage
  [Database]=Maintenance
  ['Auth Service']=MajorOutage
  ['Email Delivery']=Operational
  [Search]=Operational
  ['Beta Sandbox']=Degraded
)

# Hard failures (non-zero exit); NOTES are non-fatal observations.
FAILED=()
NOTES=()
printf '  %-18s %-14s %s\n' name expected actual
while IFS=$'\t' read -r name actual; do
  [[ -z "$name" ]] && continue
  expected="${EXPECT[$name]:-?}"
  mark="OK"
  if [[ "$actual" != "$expected" ]]; then
    mark="MISMATCH"
    FAILED+=("component '${name}': expected ${expected}, derived ${actual} — check the open-incident seed rows")
  fi
  printf '  %-18s %-14s %s [%s]\n' "$name" "$expected" "$actual" "$mark"
done <<< "$pg_states"

# Informational: raw last-5-min counters (operator drawer data; the page
# ignores these — fix-api SHOULD show errors here while rendering green).
echo
echo "## Raw last-5-min counters per public component (drawer data, page-inert)"
ch -q "
SELECT spc.public_name,
       coalesce(c.up_, 0)   AS up_,
       coalesce(c.down_, 0) AS down_,
       coalesce(c.deg_, 0)  AS deg_,
       coalesce(c.err_, 0)  AS err_
FROM postgresql('${PG_CONTAINER}:5432','monitor','status_page_components','monitor','monitor') spc
LEFT JOIN (
  SELECT target_id,
         countIf(status='up')       AS up_,
         countIf(status='down')     AS down_,
         countIf(status='degraded') AS deg_,
         countIf(status='error')    AS err_
  FROM monitor.check_results
  WHERE timestamp >= now() - INTERVAL 5 MINUTE
  GROUP BY target_id
) c ON c.target_id = spc.target_id
WHERE spc.org_id=toUUID('${ORG}')
ORDER BY spc.public_name
FORMAT PrettyCompactMonoBlock" | sed 's/^/  /'


# ── HTTP smoke ───────────────────────────────────────────────────────────────
echo
echo "## Public page render"
http_url="http://${SLUG}.${BASE_DOMAIN}:8080/"
echo "  host ${SLUG}.${BASE_DOMAIN} resolves against status_pages.slug='${SLUG}' (subdomain routing must be ON)"
# Cache TTL is ~10s; give the aggregator one beat so the freshly-seeded rows
# replace any prior cached page.
sleep 12
read -r status_code time_total < <(curl -s -o /tmp/seed-fixtures-pub.html \
  -w '%{http_code} %{time_total}\n' "$http_url" 2>/dev/null || echo '000 0')
echo "  GET $http_url → HTTP $status_code (${time_total}s)"
if [[ "$status_code" == "200" ]]; then
  states=$(grep -oE '>Operational<|>Degraded<|>Partial outage<|>Major outage<|>Maintenance<' \
           /tmp/seed-fixtures-pub.html | sort | uniq -c | awk '{print $2" "$3" ×"$1}')
  archive=$(grep -c 'Older incidents' /tmp/seed-fixtures-pub.html || true)
  body_bytes=$(wc -c < /tmp/seed-fixtures-pub.html | tr -d ' ')
  echo "  body size: ${body_bytes} bytes"
  echo "  rendered badges:"
  echo "$states" | sed 's/^/    /'
  echo "  'Older incidents →' archive link present: $([[ $archive -gt 0 ]] && echo yes || echo no)"
else
  case "$status_code" in
    000) hint="connection refused — is the app running on :8080?" ;;
    303) hint="redirected (login) — subdomain routing OFF; run with UPTIMEPAGE_TENANCY__SUBDOMAIN_PUBLIC_ROUTES=true and UPTIMEPAGE_PUBLIC_STATUS__BASE_DOMAIN=${BASE_DOMAIN}" ;;
    404) hint="no enabled status page with slug '${SLUG}' — the org's public page slug must equal the subdomain label" ;;
    *)   hint="unexpected status — check application logs" ;;
  esac
  echo "  WARN: page did not return 200 — $hint"
  FAILED+=("public page render: HTTP ${status_code} — ${hint}")
fi

# ── Operator Monitors page render (group + owner + bulk markup) ─────────────
echo
echo "## Operator Monitors page render"
monitors_url="http://app.${BASE_DOMAIN}:8080/targets"
cookies_file="${HOME}/.uptimepage-dev-cookies"
curl_auth=()
if [[ -f "$cookies_file" ]]; then
  curl_auth=(-b "$cookies_file")
fi
read -r monitors_status monitors_time < <(curl -s -o /tmp/seed-fixtures-monitors.html \
  -w '%{http_code} %{time_total}\n' "${curl_auth[@]}" "$monitors_url" 2>/dev/null || echo '000 0')
echo "  GET $monitors_url → HTTP $monitors_status (${monitors_time}s)"
if [[ "$monitors_status" == "200" ]]; then
  group_count=$(grep -c 'class="monitors-group"' /tmp/seed-fixtures-monitors.html || true)
  row_count=$(grep -c 'data-row-id=' /tmp/seed-fixtures-monitors.html || true)
  bulk_present=$(grep -c 'id="monitors-bulk"' /tmp/seed-fixtures-monitors.html || true)
  avatar_count=$(grep -c 'class="monitors-avatar"' /tmp/seed-fixtures-monitors.html || true)
  ungrouped_present=$(grep -c 'data-group-name="Ungrouped"' /tmp/seed-fixtures-monitors.html || true)
  echo "    group cards rendered  : $group_count   (expected ≥ 5: API & Web, CDN, Infrastructure, Notifications, Beta, Integrations, Ungrouped)"
  echo "    monitor rows rendered : $row_count    (expected 14)"
  echo "    bulk-bar present      : $([[ $bulk_present -gt 0 ]] && echo yes || echo no)"
  echo "    owner avatars         : $avatar_count   (≥ 1 unless org has no members)"
  echo "    Ungrouped group       : $([[ $ungrouped_present -gt 0 ]] && echo yes || echo no)   (fix-search → NULL group_name)"
  if (( group_count < 5 || row_count < 14 || bulk_present < 1 )); then
    echo "    WARN: Monitors page render below expected baseline — check template + view wiring"
    FAILED+=("operator monitors render: groups=${group_count} rows=${row_count} bulk=${bulk_present} (want ≥5 / 14 / ≥1)")
  fi
else
  echo "  (page requires auth — re-run 'just dev-login' so cookies land in $cookies_file)"
  NOTES+=("operator monitors check skipped: HTTP ${monitors_status} (no dev-login cookie)")
fi

# ── Adversarial-title escape verification ────────────────────────────────────
echo
echo "## Adversarial-title escape"
if grep -q 'Adversarial title &#60;/script&#62;' /tmp/seed-fixtures-pub.html 2>/dev/null; then
  echo "  HTML body : entity-encoded ✓"
else
  echo "  HTML body : NOT FOUND or not encoded — verify XSS escape path"
  FAILED+=("adversarial-title HTML escape: encoded marker absent (public page may have failed to render)")
fi
if grep -q 'Adversarial title \\u003c/script\\u003e' /tmp/seed-fixtures-pub.html 2>/dev/null; then
  echo "  JSON blob : \\u-escaped ✓"
else
  echo "  JSON blob : NOT FOUND — day-popover JSON may not be rendering"
  NOTES+=("adversarial-title JSON escape: \\u-escaped marker absent (day-popover JSON or page render)")
fi

# ── Day-strip pattern (1 char per day, oldest→newest) ────────────────────────
echo
echo "## Day-strip pattern (left=89d ago, right=today)"
python3 - /tmp/seed-fixtures-pub.html <<'PY' || true
import re, sys
html = open(sys.argv[1]).read()
sym = {'op':'.','deg':'D','part':'p','maj':'M','mnt':'m','none':'_'}
# Match each day-cell, capture the state class.
cells = re.findall(r'day-cell day-cell--([a-z-]+)', html)
# Match component names from the <h3> headers preceding each strip.
names = re.findall(r'class="component-name[^>]*>([^<]+)<', html)
n = 90
strips = [cells[i*n:(i+1)*n] for i in range(len(cells)//n)]
for i, s in enumerate(strips):
    label = (names[i] if i < len(names) else f'comp{i}')[:14]
    print(f'  {label:14s} {"".join(sym.get(c,"?") for c in s)}')
PY

echo
echo "=========================================================================="
if (( ${#NOTES[@]} > 0 )); then
  echo "  NOTES (non-fatal):"
  for n in "${NOTES[@]}"; do echo "    • $n"; done
fi
if (( ${#FAILED[@]} == 0 )); then
  echo "  RESULT: all checks passed ✓"
else
  echo "  RESULT: ${#FAILED[@]} check(s) failed:"
  for f in "${FAILED[@]}"; do echo "    ✗ $f"; done
fi
echo "=========================================================================="
echo
echo "Seeded org '${SLUG}' (id ${ORG})."
echo "  monitors  : 14 (7 public/visible HTTP + 1 disabled public + 2 internal HTTP + 4 non-HTTP)"
echo "  groups    : API & Web · CDN · Infrastructure · Notifications · Beta · Integrations · Ungrouped"
echo "  owners    : $([[ -n "$OWNER_USER_ID" ]] && echo "8 monitors bound to ${OWNER_USER_ID:0:8}…" || echo 'no member found — every monitor unowned')"
echo "  incidents : 158 (150 resolved across 87d + 4 active (one per frozen public monitor)
              + 1 adversarial-title + 2 internal-only on non-public monitors + 1 manually declared)
  ops state : active public incidents split triggered/acknowledged; internal + manual
              incidents carry activity timelines for the operator console"
echo "  channels  : 9 (slack + webhook + whatsapp + discord + msteams + google_chat enabled; email enabled but unverified; telegram disabled; telegram_app disabled with unlink note)"
echo "  managed   : fix-payment/fix-admin + Fixture Webhook → terraform chip; fix-tcp + Fixture Slack → api chip"
echo "  alerts    : bound on fix-api / fix-db / fix-auth"
echo "  maintenance: 4 windows (1 active bound to fix-db) → drives Maintenance state"
echo "  history   : 6-day NoData gap on fix-email; per-target last-5-min divergence"
echo "  team      : +1 member (teammate@fixture.test) + 1 pending invite + second org"
echo "              'fixture-second' (renders the nav org picker)"
echo
echo "Public status page : http://${SLUG}.${BASE_DOMAIN}:8080/"
echo "Operator dashboard : http://app.${BASE_DOMAIN}:8080/"
echo "Operator monitors  : http://app.${BASE_DOMAIN}:8080/targets"
echo "Operator incidents : http://app.${BASE_DOMAIN}:8080/incidents"
echo "Operator team      : http://app.${BASE_DOMAIN}:8080/settings/team"
echo "(public page cache TTL ~10s; wait a moment before first load)"

exit $(( ${#FAILED[@]} > 0 ))
