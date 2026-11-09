#!/usr/bin/env bash
# Seed eight alert sources and fill their delivery logs through the real
# /hooks/alerts route, so every state the alert source pages render is on
# screen at once:
#
#   Prod Alertmanager   grouped; opened, repeat, resolved, ignored, unparsed
#   Grafana signed      per alert, HMAC signed; an unsigned request refused;
#                       token rotated and the old one still sending
#   Datadog             every transition: Triggered, Re-Triggered, Warn,
#                       No Data, Renotify, Recovered
#   Deploy script       generic: trigger, acknowledge, resolve, a resolve
#                       for a key never opened, a bad event_action
#   Paused              paused after its first alert; a new key is refused
#   Held                held by the plan after its first alert (until the
#                       hold sweep lifts it, since the plan covers it)
#   Alert storm         53 keys against the cap of 50: the summary incident
#                       and capped rows; its only channel is off, so it
#                       pages nobody
#   Never used          no events at all
#
# `unavailable` is the one outcome left out: it needs the database to fail.
#
# The bodies are the captured fixtures in tests/fixtures/alerts. Paging goes to
# a webhook at example.com with reminders off; the storm pages nobody.
#
# Env overrides:
#   SLUG          org slug to seed onto    (default: devorg)
#   APP           running app origin       (default: http://app.lvh.me:8080)
#   TOKEN         session cookie value     (default: devsession-localtest-0000000000)
#   PG_CONTAINER  postgres container name  (default: uptimepage-postgres-1)
#   CH_CONTAINER  clickhouse container     (default: uptimepage-clickhouse-1)
#
# Idempotent: sources and channels named "… (seeded)", their incidents and
# their delivery log rows are wiped first.
#
# Requires `just up-app` + `just dev-login` first.
set -euo pipefail

SLUG="${SLUG:-devorg}"
APP="${APP:-http://app.lvh.me:8080}"
TOKEN="${TOKEN:-devsession-localtest-0000000000}"
PG_CONTAINER="${PG_CONTAINER:-uptimepage-postgres-1}"
CH_CONTAINER="${CH_CONTAINER:-uptimepage-clickhouse-1}"
FIX="$(cd "$(dirname "${BASH_SOURCE[0]}")/../tests/fixtures/alerts" && pwd)"
SECRET="seeded-grafana-signing-secret"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

pg() { docker exec -i "$PG_CONTAINER" psql -U monitor -d monitor -v ON_ERROR_STOP=1 "$@"; }
ch() { docker exec -i "$CH_CONTAINER" clickhouse-client "$@"; }

ORG=$(pg -tAc "SELECT id FROM organizations WHERE slug='${SLUG}' AND deleted_at IS NULL")
if [[ -z "$ORG" ]]; then
  echo "error: org '${SLUG}' missing — run 'just dev-login' (or SLUG=… just dev-login) first" >&2
  exit 1
fi

# request METHOD PATH [BODY]: sets CODE and RESP.
request() {
  local out
  out=$(curl -sS -w $'\n%{http_code}' -X "$1" -b "_sm_session=${TOKEN}" \
          -H 'X-Requested-With: uptimepage' -H 'Accept: application/json' \
          -H 'Content-Type: application/json' ${3:+--data-binary "$3"} "${APP}$2")
  CODE=${out##*$'\n'}
  RESP=${out%$'\n'*}
}

# Fails the run on anything but 2xx, and prints the body (never a token).
fail() {
  echo "error: $1 → ${CODE}: ${RESP:0:300}" >&2
  exit 1
}

api() {
  request "$@"
  [[ $CODE == 2* ]] || fail "$1 $2"
  printf '%s' "$RESP"
}

if ! api GET /api/v1/alert-sources >/dev/null 2>&1; then
  echo "error: ${APP} not reachable or not signed in — run 'just up-app' + 'just dev-login' first" >&2
  exit 1
fi

# send TOKEN BODY LABEL [curl args…]: BODY is a file path or a JSON string.
send() {
  local token=$1 body=$2 label=$3 code
  shift 3
  [[ -f $body ]] && body="@${body}"
  code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "${APP}/hooks/alerts" \
           -H "Authorization: Bearer ${token}" -H 'Content-Type: application/json' \
           "$@" --data-binary "$body")
  printf '    %s  %s\n' "$code" "$label"
  sleep 0.3
}

# signed TOKEN FILE LABEL: Grafana's HMAC of "timestamp:body".
signed() {
  local ts sig
  ts=$(date +%s)
  sig=$( { printf '%s:' "$ts"; cat "$2"; } | openssl dgst -sha256 -hmac "$SECRET" -r | cut -d' ' -f1)
  send "$1" "$2" "$3" -H "X-Grafana-Alerting-Timestamp: ${ts}" -H "X-Grafana-Alerting-Signature: ${sig}"
}

# create BODY → "id token". The app caches plan limits for
# quotas.plan_cache_ttl_secs (5 min by default), so a cap just lifted can still
# refuse for a while.
create() {
  local tries=0
  while request POST /api/v1/alert-sources "$1"; [[ $CODE == 422 && $RESP == *QUOTA_EXCEEDED* ]]; do
    (( tries++ )) || echo "    waiting for the app's plan cache to see the lifted cap (up to 5 min)" >&2
    (( tries < 64 )) || break
    sleep 5
  done
  [[ $CODE == 2* ]] || fail "POST /api/v1/alert-sources"
  jq -r '"\(.id) \(.ingest_token)"' <<<"$RESP"
}

echo "==> Postgres: lift the plan gate so eight sources fit"
pg -q <<SQL
UPDATE plans SET max_alert_sources = GREATEST(max_alert_sources, 20)
 WHERE id = (SELECT a.plan_id FROM accounts a
              JOIN organizations o ON o.account_id = a.id
             WHERE o.id = '${ORG}');
SQL

echo "==> Wipe the previous run"
keep=$(pg -tAc "SELECT string_agg(format('toUUID(%L)', id), ',') FROM alert_sources
                 WHERE org_id = '${ORG}' AND name NOT LIKE '% (seeded)'")
where="org_id = toUUID('${ORG}')"
[[ -n "$keep" ]] && where+=" AND source_id NOT IN (${keep})"
ch -q "ALTER TABLE monitor.alert_events DELETE WHERE ${where} SETTINGS mutations_sync = 1"
pg -q <<SQL
DELETE FROM incidents WHERE org_id = '${ORG}' AND alert_source_name LIKE '% (seeded)';
DELETE FROM alert_sources WHERE org_id = '${ORG}' AND name LIKE '% (seeded)';
DELETE FROM notification_channels WHERE org_id = '${ORG}' AND name LIKE '% (seeded)';
SQL

echo "==> Channels: one that pages, one switched off"
LIVE=$(api POST /api/v1/notification-channels \
  '{"name":"Seeded alerts (seeded)","config":{"type":"webhook","url":"https://example.com/uptimepage-seeded-alerts"}}' \
  | jq -r '.id // .channel_id')
MUTED=$(api POST /api/v1/notification-channels \
  '{"name":"Muted (seeded)","config":{"type":"webhook","url":"https://example.com/uptimepage-seeded-muted"}}' \
  | jq -r '.id // .channel_id')
route="\"alerts\":[{\"channel_id\":\"${LIVE}\"}],\"renotify_interval_secs\":0"

echo "==> Prod Alertmanager (seeded)"
src=$(create "{\"name\":\"Prod Alertmanager (seeded)\",\"kind\":\"alertmanager\",\"grouping\":\"group\",${route}}")
read -r AM AM_TOKEN <<<"$src"
send "$AM_TOKEN" "$FIX/alertmanager_firing.json" "KubePodCrashLooping fires"
for _ in 1 2 3; do send "$AM_TOKEN" "$FIX/alertmanager_firing.json" "repeat"; done
send "$AM_TOKEN" "$FIX/alertmanager_truncated_firing.json" "truncated group"
send "$AM_TOKEN" "$FIX/alertmanager_partial.json" "api-1 recovers, the rest still fire"
send "$AM_TOKEN" "$FIX/alertmanager_resolved.json" "group resolves"
send "$AM_TOKEN" "$FIX/alertmanager_resolved.json" "resolve sent again"
send "$AM_TOKEN" "$FIX/alertmanager_truncated_resolved.json" "resolve for a group never opened"
send "$AM_TOKEN" '<html><body>502 Bad Gateway</body></html>' "an HTML error page"

echo "==> Grafana signed (seeded)"
src=$(create "{\"name\":\"Grafana signed (seeded)\",\"kind\":\"grafana\",\"grouping\":\"alert\",\"signing_secret\":\"${SECRET}\",${route}}")
read -r GF GF_TOKEN <<<"$src"
signed "$GF_TOKEN" "$FIX/grafana_firing.json" "HighErrorRate fires on two pods"
signed "$GF_TOKEN" "$FIX/grafana_datasource_error.json" "datasource error"
send "$GF_TOKEN" "$FIX/grafana_firing.json" "unsigned, refused"
signed "$GF_TOKEN" "$FIX/grafana_resolved.json" "HighErrorRate resolves"
api POST "/api/v1/alert-sources/${GF}/rotate" '{}' >/dev/null
signed "$GF_TOKEN" "$FIX/grafana_firing.json" "old token after rotation, still accepted"

echo "==> Datadog (seeded)"
src=$(create "{\"name\":\"Datadog (seeded)\",\"kind\":\"datadog\",${route}}")
read -r DD DD_TOKEN <<<"$src"
dd() {
  send "$DD_TOKEN" "{\"transition\":\"$1\",\"cycle_key\":\"$2\",\"monitor_id\":\"$3\",\"scope\":\"env:prod\",\"title\":\"[$1] $4\",\"message\":\"$4 on env:prod\",\"priority\":\"$5\",\"link\":\"https://app.datadoghq.com/monitors/$3\",\"tags\":\"env:prod,team:payments\",\"date\":\"$(date +%s)000\"}" "$1 $4"
}
dd "Triggered" "cyc-checkout" "41001" "Checkout p99 above 800 ms" "P1"
dd "Re-Triggered" "cyc-checkout" "41001" "Checkout p99 above 800 ms" "P1"
dd "Warn" "cyc-disk" "41002" "Disk on db-1 above 80%" "P3"
dd "No Data" "cyc-queue" "41003" "Billing queue depth" "P2"
dd "Renotify" "cyc-disk" "41002" "Disk on db-1 above 80%" "P3"
dd "Recovered" "cyc-checkout" "41001" "Checkout p99 above 800 ms" "P1"

echo "==> Deploy script (seeded)"
src=$(create "{\"name\":\"Deploy script (seeded)\",\"kind\":\"generic\",${route}}")
read -r GN GN_TOKEN <<<"$src"
send "$GN_TOKEN" "$FIX/generic_trigger.json" "trigger checkout-5xx"
send "$GN_TOKEN" '{"event_action":"acknowledge","dedup_key":"checkout-5xx"}' "acknowledge"
send "$GN_TOKEN" '{"event_action":"trigger","dedup_key":"deploy-api-v2","payload":{"summary":"Deploy of api v2 rolled back","source":"ci","severity":"warning"}}' "trigger deploy-api-v2"
send "$GN_TOKEN" '{"event_action":"resolve","dedup_key":"checkout-5xx"}' "resolve checkout-5xx"
send "$GN_TOKEN" '{"event_action":"resolve","dedup_key":"never-opened"}' "resolve a key never opened"
send "$GN_TOKEN" '{"event_action":"explode","dedup_key":"x"}' "unknown event_action"

echo "==> Paused (seeded)"
src=$(create "{\"name\":\"Paused (seeded)\",\"kind\":\"generic\",${route}}")
read -r PS PS_TOKEN <<<"$src"
send "$PS_TOKEN" '{"event_action":"trigger","dedup_key":"nightly-backup","payload":{"summary":"Nightly backup failed","severity":"error"}}' "trigger nightly-backup"
api PATCH "/api/v1/alert-sources/${PS}" '{"enabled":false}' >/dev/null
send "$PS_TOKEN" '{"event_action":"trigger","dedup_key":"weekly-report","payload":{"summary":"Weekly report did not send","severity":"warning"}}' "new key while paused"
send "$PS_TOKEN" '{"event_action":"trigger","dedup_key":"nightly-backup","payload":{"summary":"Nightly backup failed","severity":"error"}}' "open key while paused"

echo "==> Held (seeded)"
src=$(create "{\"name\":\"Held (seeded)\",\"kind\":\"alertmanager\",\"grouping\":\"group\",${route}}")
read -r HD HD_TOKEN <<<"$src"
send "$HD_TOKEN" "$FIX/alertmanager_firing.json" "KubePodCrashLooping fires"
pg -q -c "UPDATE alert_sources SET plan_hold_at = now() WHERE id = '${HD}'"
jq '.groupKey = "{}/{}:{alertname=\"NodeDiskPressure\"}"
    | .groupLabels.alertname = "NodeDiskPressure"
    | .commonLabels.alertname = "NodeDiskPressure"
    | .alerts[].labels.alertname = "NodeDiskPressure"' \
  "$FIX/alertmanager_firing.json" > "$TMP/held_new.json"
send "$HD_TOKEN" "$TMP/held_new.json" "new group while held"
send "$HD_TOKEN" "$FIX/alertmanager_resolved.json" "open group resolves while held"

echo "==> Alert storm (seeded)"
src=$(create "{\"name\":\"Alert storm (seeded)\",\"kind\":\"generic\",\"alerts\":[{\"channel_id\":\"${MUTED}\"}],\"renotify_interval_secs\":0}")
read -r ST ST_TOKEN <<<"$src"
pg -q -c "UPDATE notification_channels SET enabled = false WHERE id = '${MUTED}'"
for i in $(seq -w 1 53); do
  code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "${APP}/hooks/alerts" \
           -H "Authorization: Bearer ${ST_TOKEN}" -H 'Content-Type: application/json' \
           --data-binary "{\"event_action\":\"trigger\",\"dedup_key\":\"edge-${i}\",\"payload\":{\"summary\":\"Edge node ${i} unreachable\",\"severity\":\"critical\"}}")
  [[ $code == 2* ]] || echo "    ${code}  edge-${i}"
done
echo "    53 triggers sent"

echo "==> Never used (seeded)"
create "{\"name\":\"Never used (seeded)\",\"kind\":\"datadog\",${route}}" >/dev/null

echo
echo "==> Delivery log by source and outcome"
sleep 1
for pair in "Prod Alertmanager=${AM}" "Grafana signed=${GF}" "Datadog=${DD}" \
            "Deploy script=${GN}" "Paused=${PS}" "Held=${HD}" "Alert storm=${ST}"; do
  ch -q "SELECT '${pair%%=*}', outcome, count() FROM monitor.alert_events
         WHERE org_id = toUUID('${ORG}') AND source_id = toUUID('${pair#*=}')
         GROUP BY outcome ORDER BY outcome FORMAT TSV"
done | column -t -s $'\t'
echo
echo "Open ${APP}/settings/alert-sources"
