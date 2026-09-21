#!/usr/bin/env bash
# Smoke test for the email alert-channel hardening bundle.
# Prereq: `just up-app` (app serving on app.lvh.me:8080). Nothing else — this
# script seeds its own owner session and cleans up after itself.
#
# Covers: member fast-verify (#4/#8), third-party gating, reserved+operator
# domain block incl. subdomain parent-walk (#5), and the /alert-channel/stop
# CSRF exemption being open where intended and closed everywhere else (#1/#2).
#
# Overridable env: BASE, COOKIE, PG, FROM_DOMAIN.
set -uo pipefail

BASE="${BASE:-http://app.lvh.me:8080}"
COOKIE="${COOKIE:-_sm_session=devsession-localtest-0000000000}"
PG="${PG:-uptimepage-postgres-1}"
FROM_DOMAIN="${FROM_DOMAIN:-example.invalid}"   # operator mail domain (compose.dev.yml from_address)
XRW='X-Requested-With: uptimepage'
DEAD_UUID='00000000-0000-0000-0000-0000000000ff'

pass=0; fail=0
ok(){ printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass+1)); }
no(){ printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail+1)); }
hd(){ printf '\n\033[1m%s\033[0m\n' "$1"; }
expect(){ if [ "$2" = "$3" ]; then ok "$1 ($2)"; else no "$1 (got '$2', want '$3')"; fi; }

db(){ docker exec -i "$PG" psql -U monitor -d monitor -tAc "$1" </dev/null 2>/dev/null | tr -d '[:space:]'; }

# POST a new email channel; sets HTTP (status) + BODY (response body)
HTTP=''; BODY=''
post_channel(){
  local r; r=$(curl -s -w $'\n%{http_code}' -b "$COOKIE" -H "$XRW" \
        -H 'Content-Type: application/json' \
        -X POST "$BASE/api/v1/notification-channels" \
        -d "{\"name\":\"$1\",\"config\":{\"type\":\"email\",\"to\":\"$2\"}}")
  HTTP=$(tail -1 <<<"$r"); BODY=$(sed '$d' <<<"$r")
}
blocked(){ # label — asserts the abuse block fired
  if grep -q EMAIL_DESTINATION_BLOCKED <<<"$BODY"; then ok "$1 (blocked)";
  else no "$1 (status $HTTP, body: ${BODY:0:80})"; fi; }

# ── preflight ───────────────────────────────────────────────────────────────
hd 'Preflight'
if [ "$(curl -s -o /dev/null -w '%{http_code}' -b "$COOKIE" "$BASE/api/v1/notification-channels")" = "000" ]; then
  echo "  app not reachable at $BASE — run 'just up-app' first"; exit 2
fi
bash scripts/seed-dev-session.sh >/dev/null 2>&1 || true
expect 'owner API reachable + authed' \
  "$(curl -s -o /dev/null -w '%{http_code}' -b "$COOKIE" "$BASE/api/v1/notification-channels")" '200'
db "DELETE FROM channel_verification_tokens WHERE channel_id IN (SELECT id FROM notification_channels WHERE name LIKE 'smoke-%')" >/dev/null
db "DELETE FROM notification_channels WHERE name LIKE 'smoke-%'" >/dev/null
expect 'stop secret provisioned at boot' "$(db "SELECT count(*) FROM app_secrets WHERE name='alert_channel_stop'")" '1'

# ── A: org member email auto-verifies (#4) ──────────────────────────────────
hd 'A  member email skips verification (#4)'
post_channel smoke-member dev@local.test
expect 'create channel to owner email' "$HTTP" '201'
va=$(db "SELECT COALESCE(verified_at::text,'null') FROM notification_channels WHERE name='smoke-member' ORDER BY created_at DESC LIMIT 1")
{ [ -n "$va" ] && [ "$va" != 'null' ]; } && ok "verified_at set immediately" || no "verified_at not set (got '$va')"
expect 'no verification mail minted' \
  "$(db "SELECT count(*) FROM channel_verification_tokens t JOIN notification_channels c ON c.id=t.channel_id WHERE c.name='smoke-member'")" '0'

# ── B: third-party address still gated ──────────────────────────────────────
hd 'B  stranger address still requires confirmation'
post_channel smoke-stranger nobody-smoke@example.com
expect 'create channel to stranger' "$HTTP" '201'
expect 'stays unverified' \
  "$(db "SELECT COALESCE(verified_at::text,'null') FROM notification_channels WHERE name='smoke-stranger' ORDER BY created_at DESC LIMIT 1")" 'null'
tb=0
for _ in 1 2 3 4 5 6; do
  tb=$(db "SELECT count(*) FROM channel_verification_tokens t JOIN notification_channels c ON c.id=t.channel_id WHERE c.name='smoke-stranger'")
  [ "$tb" = '1' ] && break; sleep 0.4
done
expect 'verification mail minted (async)' "$tb" '1'

# ── C: reserved + operator-domain block (#5) ────────────────────────────────
hd 'C  reserved + operator-domain block (#5)'
post_channel smoke-role postmaster@example.com;     blocked 'role mailbox postmaster@'
post_channel smoke-op   "security@$FROM_DOMAIN";     blocked 'operator apex domain'
post_channel smoke-sub  "alerts@sub.$FROM_DOMAIN";   blocked 'operator subdomain (parent-walk)'
post_channel smoke-ok   oncall@customer.example
expect 'ordinary address allowed' "$HTTP" '201'

# ── D: stop route + CSRF exemption (#1/#2) ──────────────────────────────────
hd 'D  /alert-channel/stop CSRF exemption (#2)'
CID=$(db "SELECT id FROM notification_channels WHERE name='smoke-member' LIMIT 1")
if [ -z "$CID" ]; then
  no 'no channel id — skipping D'
else
  expect 'GET bad token → invalid page' \
    "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/alert-channel/stop?c=$CID&t=bogus")" '404'

  # logged-in POST (cookie, NO X-Requested-With) must NOT be CSRF-blocked
  r2=$(curl -s -w $'\n%{http_code}' -b "$COOKIE" -X POST "$BASE/alert-channel/stop?c=$CID&t=bogus")
  c2=$(tail -1 <<<"$r2"); b2=$(sed '$d' <<<"$r2")
  { [ "$c2" != '403' ] && ! grep -q CSRF_PROTECTION <<<"$b2"; } \
    && ok "logged-in POST not CSRF-blocked (status $c2)" \
    || no "logged-in POST CSRF-blocked (status $c2)"
  expect 'bad-token POST left channel enabled' "$(db "SELECT enabled FROM notification_channels WHERE id='$CID'")" 't'

  # contrast — a normal cookie DELETE without the header IS blocked
  r3=$(curl -s -w $'\n%{http_code}' -b "$COOKIE" -X DELETE "$BASE/api/v1/notification-channels/$DEAD_UUID")
  c3=$(tail -1 <<<"$r3"); b3=$(sed '$d' <<<"$r3")
  { [ "$c3" = '403' ] && grep -q CSRF_PROTECTION <<<"$b3"; } \
    && ok 'guard still active off the exempt path (403)' \
    || no "guard NOT active off exempt path (status $c3)"
fi

# ── cleanup ─────────────────────────────────────────────────────────────────
db "DELETE FROM channel_verification_tokens WHERE channel_id IN (SELECT id FROM notification_channels WHERE name LIKE 'smoke-%')" >/dev/null
db "DELETE FROM notification_channels WHERE name LIKE 'smoke-%'" >/dev/null

hd "Result: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
