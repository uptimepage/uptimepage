#!/usr/bin/env bash
# Walk through how incidents, the recovery period and the status page behave,
# on the local stack.
#
#   seed   a published status page with a month of history: a flapping
#          service folded into one past row, a partial outage the day colour
#          weighs at 30%, a degraded day, services that failed together, an
#          incident that was back up for a while before failing again, one an
#          operator wrote about, and older ones behind the toggle
#   live   a tiny local service that goes down, comes back, fails again inside
#          the recovery period and then stays up, watched by a real monitor:
#          the incident opens, reads Monitoring while it recovers, keeps the
#          stretch it was back up in when it relapses, and closes once the
#          recovery has held
#   all    both (the default)
#
# Needs `just up` and the app running natively with private targets allowed,
# since every demo monitor probes 127.0.0.1:
#
#   UPTIMEPAGE_SECURITY__ALLOW_PRIVATE_TARGETS=true just run
#
# Env overrides:
#   SLUG          org slug                  (default: devorg)
#   PORT          demo service port         (default: 18085)
#   HOLD          live monitor's hold, secs (default: 180, the product default)
#   APP           app base URL              (default: http://127.0.0.1:8080)
#   PG_CONTAINER  postgres container name   (default: uptimepage-postgres-1)
#   CH_CONTAINER  clickhouse container      (default: uptimepage-clickhouse-1)
#
# Idempotent: the demo monitors are tagged `demo-recovery`; their incidents,
# Postgres rows, ClickHouse history and the `demo-recovery` page are wiped
# before re-insert. Ctrl-C stops the service and pauses the demo monitors.
set -euo pipefail

MODE="${1:-all}"
SLUG="${SLUG:-devorg}"
PORT="${PORT:-18085}"
HOLD="${HOLD:-180}"
APP="${APP:-http://127.0.0.1:8080}"
PG_CONTAINER="${PG_CONTAINER:-uptimepage-postgres-1}"
CH_CONTAINER="${CH_CONTAINER:-uptimepage-clickhouse-1}"
TAG=demo-recovery

case "$MODE" in
  seed | live | all) ;;
  *)
    echo "usage: $0 [seed|live|all]" >&2
    exit 2
    ;;
esac

pg() { docker exec -i "$PG_CONTAINER" psql -U monitor -d monitor -v ON_ERROR_STOP=1 "$@"; }
ch() { docker exec -i "$CH_CONTAINER" clickhouse-client "$@"; }
source "$(dirname "${BASH_SOURCE[0]}")/lib/seed-purge.sh"

if ! curl -fsS "${APP}/healthz" >/dev/null 2>&1; then
  echo "error: the app is not answering at ${APP}. Start it with:" >&2
  echo "         UPTIMEPAGE_SECURITY__ALLOW_PRIVATE_TARGETS=true just run" >&2
  exit 1
fi

ORG=$(pg -tAc "SELECT id FROM organizations WHERE slug='${SLUG}' AND deleted_at IS NULL")
if [[ -z "$ORG" ]]; then
  echo "error: org '${SLUG}' missing — run 'just dev-login' first" >&2
  exit 1
fi
REGION=$(pg -tAc "SELECT id FROM regions WHERE enabled ORDER BY id LIMIT 1")
if [[ -z "$REGION" ]]; then
  echo "error: no enabled region — bring the stack up first" >&2
  exit 1
fi

# ── the demo service ───────────────────────────────────────────────────────
# /ok always answers 200, so the seeded monitors stay green under real probes.
# /flap follows the timeline from its first request: down, back up for half
# the hold, down again inside the hold, then up for good.
SERVICE_PID=""
start_service() {
  python3 - "$PORT" "$HOLD" <<'PY' &
import http.server, sys, time

port, hold = int(sys.argv[1]), int(sys.argv[2])
timeline = [(0, False), (60, True), (60 + hold // 2, False), (100 + hold // 2, True)]
start = None
said = None

def up_now():
    global start, said
    if start is None:
        start = time.time()
    t = time.time() - start
    up = [u for at, u in timeline if t >= at][-1]
    if up != said:
        said = up
        print(f"  [service t+{int(t):>3}s] {'UP   — answering 200' if up else 'DOWN — answering 503'}", flush=True)
    return up

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        ok = self.path.startswith("/ok") or up_now()
        self.send_response(200 if ok else 503)
        self.send_header("content-type", "text/plain")
        self.end_headers()
        self.wfile.write(b"ok\n" if ok else b"down\n")

    def log_message(self, *args):
        pass

http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY
  SERVICE_PID=$!
  sleep 0.5
  if ! curl -fsS "http://127.0.0.1:${PORT}/ok" >/dev/null; then
    echo "error: the demo service did not start on port ${PORT}" >&2
    exit 1
  fi
}

stop() {
  [[ -n "$SERVICE_PID" ]] && kill "$SERVICE_PID" 2>/dev/null || true
  # Without the service every demo monitor would fail and open incidents.
  pg -qc "UPDATE targets SET enabled = false WHERE org_id = '${ORG}' AND tags @> ARRAY['${TAG}']" \
    >/dev/null 2>&1 || true
  echo
  echo "Demo service stopped; the demo monitors are paused. Run again to resume."
}
trap stop EXIT

spec() {
  printf '{"type":"http","url":"http://127.0.0.1:%s/%s","method":"GET","headers":{},"body":null,"timeout":5000,"follow_redirects":false,"max_redirects":0,"expected_status":{"kind":"exact","value":200},"expected_body_contains":null,"verify_tls":true,"basic_auth":null,"bearer_token":null}' "$PORT" "$1"
}

# ── wipe the previous run ──────────────────────────────────────────────────
echo "==> Wiping the previous demo"
pg -q <<SQL
-- A monitor's delete keeps its incidents, so they go first.
DELETE FROM incidents WHERE org_id = '${ORG}'
   AND target_id IN (SELECT id FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['${TAG}']);
DELETE FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['${TAG}'];
DELETE FROM status_pages WHERE org_id = '${ORG}' AND slug = '${TAG}';
SQL

start_service

echo "==> Creating the demo monitors and status page"
pg -q <<SQL
INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled, tags, group_name,
                     alert_confirmations, region_policy, recovery_period_secs)
VALUES
  ('${ORG}', 'Webmail',       '$(spec ok)'::jsonb,   60, true, ARRAY['${TAG}','demo-webmail'], 'Demo', 2, '"any"'::jsonb, 180),
  ('${ORG}', 'Website',       '$(spec ok)'::jsonb,   60, true, ARRAY['${TAG}','demo-website'], 'Demo', 2, '"any"'::jsonb, 180),
  ('${ORG}', 'API',           '$(spec ok)'::jsonb,   60, true, ARRAY['${TAG}','demo-api'],     'Demo', 2, '"any"'::jsonb, 180),
  ('${ORG}', 'Mail relay',    '$(spec ok)'::jsonb,   60, true, ARRAY['${TAG}','demo-relay'],   'Demo', 2, '"any"'::jsonb, 180),
  ('${ORG}', 'Control panel', '$(spec ok)'::jsonb,   60, true, ARRAY['${TAG}','demo-panel'],   'Demo', 2, '"any"'::jsonb, 180),
  ('${ORG}', 'DNS',           '$(spec ok)'::jsonb,   60, true, ARRAY['${TAG}','demo-dns'],     'Demo', 2, '"any"'::jsonb, 180),
  ('${ORG}', 'Live service',  '$(spec flap)'::jsonb, 10, false, ARRAY['${TAG}','demo-live'],   'Demo', 2, '"any"'::jsonb, ${HOLD});

INSERT INTO target_regions (target_id, region)
SELECT id, '${REGION}' FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['${TAG}'];

INSERT INTO status_pages (org_id, slug, name, enabled, public_display_name, public_about)
VALUES ('${ORG}', '${TAG}', 'Demo status', true, 'Demo status',
        'A month of seeded history, and one live service you can watch.');

INSERT INTO status_page_components (org_id, status_page_id, target_id, public_name, public_group, sort_order)
SELECT '${ORG}', (SELECT id FROM status_pages WHERE org_id = '${ORG}' AND slug = '${TAG}'),
       t.id, t.name, 'Services', row_number() OVER (ORDER BY t.created_at, t.name)
  FROM targets t WHERE t.org_id = '${ORG}' AND t.tags @> ARRAY['${TAG}'];
SQL

FIRST_PAGE=$(pg -tAc "SELECT slug FROM status_pages WHERE org_id = '${ORG}' ORDER BY created_at, id LIMIT 1")
if [[ "$FIRST_PAGE" != "$TAG" ]]; then
  echo "    note: ${APP}/status shows this org's oldest page ('${FIRST_PAGE}'), not the demo one."
  echo "          Delete or disable that page to see the demo there."
fi

id_of() { pg -tAc "SELECT id FROM targets WHERE org_id='${ORG}' AND tags @> ARRAY['$1']"; }

# ── seeded history ─────────────────────────────────────────────────────────
# One line per incident: monitor tag; days ago; start (UTC); minutes; check
# status; a region still up (partial outage) or -; back up from/to as minutes
# into the incident, or -. Checks fail inside each outage, and pass elsewhere
# and inside a stretch it was back up in.
INCIDENTS="
demo-webmail;4;04:31;19;down;-;-
demo-webmail;4;04:57;24;down;-;-
demo-webmail;4;05:27;4;down;eu-west;-
demo-webmail;4;05:35;3;down;eu-west;-
demo-webmail;4;06:26;5;down;-;-
demo-webmail;4;06:37;4;down;-;-
demo-webmail;4;06:47;3;down;eu-west;-
demo-website;2;10:00;40;down;-;5-30
demo-api;3;14:00;90;down;eu-west;-
demo-relay;5;09:00;180;degraded;-;-
demo-panel;6;13:21;16;down;-;-
demo-dns;6;13:22;16;down;-;-
demo-api;6;13:21;17;down;-;-
demo-api;1;09:00;20;down;-;-
demo-website;12;08:15;30;down;-;-
demo-webmail;20;22:10;6;down;-;-
demo-webmail;20;22:40;9;down;-;-
"

declare -A DOWN_WINDOWS
if [[ "$MODE" != live ]]; then
  echo "==> Seeding a month of incidents"
  SQL=""
  while IFS=';' read -r tag days at mins status up_region stretch; do
    [[ -z "$tag" ]] && continue
    start="date_trunc('day', now()) - interval '${days} days' + interval '${at}'"
    end="${start} + interval '${mins} minutes'"
    regions_up="'{}'"
    [[ "$up_region" != - ]] && regions_up="ARRAY['${up_region}']"
    from="'{}'::timestamptz[]" until="'{}'::timestamptz[]"
    if [[ "$stretch" != - ]]; then
      from="ARRAY[${start} + interval '${stretch%-*} minutes']"
      until="ARRAY[${start} + interval '${stretch#*-} minutes']"
    fi
    SQL+="INSERT INTO incidents (org_id, target_id, started_at, ended_at, duration_secs, status_at_start,
            check_count, state, visibility, origin, regions_down, regions_up, recovered_from, recovered_until)
          VALUES ('${ORG}', (SELECT id FROM targets WHERE org_id='${ORG}' AND tags @> ARRAY['${tag}']),
            ${start}, ${end}, ${mins} * 60, '${status}', 2, 'resolved', 'public', 'monitor',
            ARRAY['${REGION}'], ${regions_up}, ${from}, ${until});
"
  done <<<"$INCIDENTS"
  pg -q <<<"$SQL"

  # An operator wrote about this one, so it keeps its own row and its words.
  pg -q <<SQL
INSERT INTO incident_updates (org_id, incident_id, phase, message, author, generated, posted_at)
SELECT '${ORG}', i.id, 'resolved', 'A bad deploy; we rolled it back.', 'Demo operator', false,
       i.ended_at
  FROM incidents i
 WHERE i.org_id = '${ORG}' AND i.target_id = '$(id_of demo-api)'
   AND i.started_at = date_trunc('day', now()) - interval '1 days' + interval '09:00';
SQL

  echo "==> ClickHouse: a month of checks every 5 minutes"
  reset_seeded_history "$ORG" "$TAG"
  for tag in demo-webmail demo-website demo-api demo-relay demo-panel demo-dns; do
    id=$(id_of "$tag")
    # The stretches each incident was down, as unix-second pairs.
    windows=$(pg -tAc "
      SELECT coalesce(string_agg(format('(%s,%s,%L)', extract(epoch FROM a)::bigint,
                                        extract(epoch FROM b)::bigint, status_at_start), ','), '')
        FROM incidents i,
             LATERAL (SELECT started_at AS a, coalesce(recovered_from[1], ended_at) AS b
                      UNION ALL
                      SELECT recovered_until[1], ended_at WHERE cardinality(recovered_until) > 0) w
       WHERE i.org_id = '${ORG}' AND i.target_id = '${id}'")
    [[ -z "$windows" ]] && windows="(0,0,'up')"
    ch -mn <<SQL
INSERT INTO monitor.check_results
  (org_id,target_id,region,timestamp,agent_id,status,duration_ms,dns_ms,connect_ms,tls_ms,ttfb_ms,response_code,error)
WITH [${windows}] AS w,
     toUnixTimestamp(now() - toIntervalMinute(number * 5)) AS ts,
     arrayFirst(x -> ts >= x.1 AND ts < x.2, w) AS hit
SELECT
  toUUID('${ORG}'), toUUID('${id}'), '${REGION}', fromUnixTimestamp(ts), 'demo-recovery',
  if(hit.1 = 0, 'up', hit.3),
  if(hit.1 = 0, toUInt32(180 + (number % 60)), if(hit.3 = 'degraded', 2400, 5000)),
  toUInt16(6), toUInt16(40), toUInt16(30), toUInt16(90),
  if(hit.1 = 0 OR hit.3 = 'degraded', 200, 503),
  if(hit.1 = 0 OR hit.3 = 'degraded', '', 'HTTP 503')
FROM numbers(30 * 24 * 12);
SQL
  done
fi

PAGE="${APP}/status"
cat <<EOF

What to look at:
  status page   ${PAGE}
                - Webmail's seven blips four days ago fold into one row; the day
                  reads orange (55 weighted minutes), not red
                - API's 90-minute partial outage three days ago counts 27 minutes
                - Mail relay's degraded day is yellow and leaves uptime alone
                - Control panel, DNS and API failed together six days ago: one row
                - Website two days ago lasted 40 minutes but was back up for 25 of
                  them, so it reads 15 minutes down
                - API yesterday keeps its own row with what the operator wrote
                - older incidents sit behind "earlier incidents"
  archive       ${PAGE}/incidents
  dashboard     ${APP}/dashboard
  incidents     ${APP}/incidents
EOF

if [[ "$MODE" == seed ]]; then
  echo
  echo "The demo service answers on port ${PORT} so the seeded monitors stay up."
  echo "Ctrl-C stops it and pauses the demo monitors."
  wait "$SERVICE_PID"
  exit 0
fi

# ── live ───────────────────────────────────────────────────────────────────
LIVE=$(id_of demo-live)
pg -qc "UPDATE targets SET enabled = true, updated_at = now() WHERE id = '${LIVE}'"

cat <<EOF

==> Live: the "Live service" monitor checks every 10s, 2 failures to open,
    a ${HOLD}s recovery period. The service goes down now, comes back after a
    minute, fails again inside the hold and then stays up. Expect about
    $(( (100 + HOLD / 2 + HOLD + 60) / 60 )) minutes. Watch:
      ${PAGE}                       (Live service: red, then green + Monitoring)
      ${APP}/incidents              (recovering beside Triggered)
      ${APP}/targets/${LIVE}/incidents

    The scheduler picks the monitor up within 30s.
EOF

last=""
started=$(date +%s)
while true; do
  row=$(pg -tAc "
    SELECT state, ended_at IS NOT NULL, recovering_since IS NOT NULL,
           cardinality(recovered_from),
           to_char(started_at AT TIME ZONE 'UTC', 'HH24:MI:SS'),
           coalesce(to_char(recovering_since AT TIME ZONE 'UTC', 'HH24:MI:SS'), ''),
           coalesce(to_char(ended_at AT TIME ZONE 'UTC', 'HH24:MI:SS'), ''),
           coalesce((SELECT string_agg(to_char(f AT TIME ZONE 'UTC', 'HH24:MI:SS') || '–'
                                       || to_char(u AT TIME ZONE 'UTC', 'HH24:MI:SS'), ', ')
                       FROM unnest(recovered_from, recovered_until) AS s(f, u)), '')
      FROM incidents WHERE target_id = '${LIVE}' ORDER BY started_at DESC LIMIT 1")
  elapsed=$(( $(date +%s) - started ))
  if [[ "$row" != "$last" ]]; then
    IFS='|' read -r state closed recovering stretches s_at r_at e_at kept <<<"$row"
    if [[ -z "$row" ]]; then
      :
    elif [[ "$closed" == t ]]; then
      echo "  [writer  t+${elapsed}s] incident CLOSED, ended at ${e_at} UTC (where the recovery began)"
      echo "                    back-up stretch kept, not downtime: ${kept:-none}"
      break
    elif [[ "$recovering" == t ]]; then
      echo "  [writer  t+${elapsed}s] incident RECOVERING since ${r_at} UTC: paging paused, page shows Monitoring"
      [[ "$stretches" != 0 ]] && echo "                    back-up stretch kept, not downtime: ${kept}"
    else
      echo "  [writer  t+${elapsed}s] incident OPEN (${state}) since ${s_at} UTC"
      [[ "$stretches" != 0 ]] && echo "                    relapsed inside the hold — same incident, stretch kept: ${kept}"
    fi
    last="$row"
  fi
  if (( elapsed >= 75 )) && [[ -z "$row" && -z "${warned:-}" ]]; then
    warned=1
    err=$(ch -q "SELECT error FROM monitor.check_results WHERE target_id = toUUID('${LIVE}')
                  ORDER BY timestamp DESC LIMIT 1" 2>/dev/null || true)
    if [[ -z "$err" ]]; then
      echo "  still no checks for the live monitor — is the app's scheduler running?"
    else
      echo "  the live monitor's checks fail with: ${err}"
      echo "  if that names a private address, restart the app with"
      echo "    UPTIMEPAGE_SECURITY__ALLOW_PRIVATE_TARGETS=true just run"
    fi
  fi
  if (( elapsed > 30 * 60 )); then
    echo "  gave up after 30 minutes"
    break
  fi
  sleep 3
done

echo
echo "Done. The pages stay up while this runs; Ctrl-C stops the service and"
echo "pauses the demo monitors."
wait "$SERVICE_PID"
