#!/usr/bin/env bash
# Stand up (or tear down) the local multi-region topology: the dev control
# plane plus three real regional agents running in docker.
#
#   scripts/dev-regions.sh         # up: mint regions + agents, start containers
#   scripts/dev-regions.sh down    # delete the harness agents/regions + stop
#
# The harness owns the region ids (eu-helsinki, apac-sg, us-east). eu-helsinki
# doubles as the control plane's co-located home region (dashboard mode probes
# nothing in-process). scripts/seed-fixtures.sh seeds fixture data onto whatever
# regions exist here — it no longer invents its own.
#
# Up mints a fresh agent token per region via the operator API, writes the
# tokens to .dev-agents.env (gitignored), then starts the agent containers from
# compose.dev.agents.yml. Idempotent: re-running deletes the previous harness
# agents first, so tokens never accumulate.
#
# Prereqs: the dev control plane is up (`just up-app`, or native `just run`
# with UPTIMEPAGE_SERVER__API_BIND=0.0.0.0:8080) and the operator surface is on
# (UPTIMEPAGE_OPERATOR__ADMIN_TOKEN set — compose.dev.yml sets a dev value).
#
# Env overrides:
#   BASE            control-plane URL the SCRIPT calls (default: http://app.lvh.me:8080)
#   OPERATOR_TOKEN  operator secret                    (default: dev-operator-token)
#   AGENT_CONTROL_PLANE_URL  URL the AGENT CONTAINERS use to reach the control
#                   plane. Defaults to http://app.lvh.me:8080 (the app host
#                   host isolation accepts), resolved to the host's :8080 by
#                   the extra_hosts host-gateway mapping. Set to override.
set -euo pipefail

ACTION="${1:-up}"
BASE="${BASE:-http://app.lvh.me:8080}"
TOKEN="${OPERATOR_TOKEN:-dev-operator-token}"
ENV_FILE=".dev-agents.env"
COMPOSE=(docker compose -f compose.dev.yml -f compose.dev.agents.yml)
AUTH=(-H "Authorization: Bearer ${TOKEN}")

# region id : agent name : env var holding the minted token
# region:agent-name:token-var:city:country:continent:lat:lon
REGIONS=(
  "eu-helsinki:dev-harness-eu-helsinki:AGENT_EU_TOKEN:Helsinki:FI:europe:60.17:24.94"
  "apac-sg:dev-harness-apac-sg:AGENT_APAC_TOKEN:Singapore:SG:asia:1.35:103.82"
  "us-east:dev-harness-us-east:AGENT_US_TOKEN:Ashburn:US:north_america:39.04:-77.49"
)

# Control-plane host:port the SCRIPT dials — used to inspect who is actually
# bound to the port when the operator handshake fails.
CP_HOSTPORT=${BASE#*://}; CP_HOSTPORT=${CP_HOSTPORT%%/*}
CP_PORT=${CP_HOSTPORT##*:}
[[ $CP_PORT == "$CP_HOSTPORT" ]] && CP_PORT=80

ts()   { date +%H:%M:%S; }
log()  { printf '%s  %s\n'        "$(ts)" "$*"; }
warn() { printf '%s  warn: %s\n'  "$(ts)" "$*" >&2; }
die()  { printf '%s  error: %s\n' "$(ts)" "$*" >&2; exit 1; }

# Non-secret fingerprint of the operator token: length + a short sha so two
# tokens can be compared in logs without ever printing the secret.
token_fp() {
  local h; h=$(printf '%s' "$1" | shasum -a 256 2>/dev/null | cut -c1-8)
  printf 'len=%s sha256=%s…' "${#1}" "${h:-unavailable}"
}

# Where the script's token came from — pinpoints a stray exported override,
# the usual cause of a same-server 401.
token_origin() {
  [[ -n ${OPERATOR_TOKEN:-} ]] && { echo "OPERATOR_TOKEN env"; return; }
  echo "script default"
}

# URL the AGENT CONTAINERS use to reach the control plane. Agents dial the app
# host (`app.lvh.me`), not the compose service name, because host isolation
# 404s any Host it can't classify as the operator surface. The
# `extra_hosts: app.lvh.me:host-gateway` mapping points it at the host's :8080.
# AGENT_CONTROL_PLANE_URL overrides.
resolve_agent_cp_url() {
  if [[ -n ${AGENT_CONTROL_PLANE_URL:-} ]]; then
    echo "$AGENT_CONTROL_PLANE_URL"
  else
    echo "http://app.lvh.me:8080"
  fi
}

# Processes LISTENing on the control-plane port, one "pid <n> (<cmd>)" per line.
# Empty when lsof is missing or nothing is bound.
port_listeners() {
  command -v lsof >/dev/null || return 0
  # `|| true`: lsof exits non-zero when nothing is bound, which would trip
  # `set -e` at the caller's `x=$(port_listeners)` assignment (no listener is a
  # normal preflight state, not an error).
  lsof -nP -iTCP:"$CP_PORT" -sTCP:LISTEN 2>/dev/null \
    | awk 'NR>1 {printf "pid %s (%s)\n", $2, $1}' | sort -u || true
}

# Dump listeners to stderr with a hint. A second listener is the classic
# culprit: a stale dev server shadows the intended one for localhost connects.
report_listeners() {
  local ls; ls=$(port_listeners)
  if [[ -n $ls ]]; then
    local n; n=$(printf '%s\n' "$ls" | grep -c .)
    warn "$n listener(s) on port $CP_PORT:"
    while IFS= read -r l; do warn "  $l"; done <<<"$ls"
    if (( n > 1 )); then
      warn "more than one listener — a stale process is shadowing the intended server"
    fi
  else
    warn "no listener found on port $CP_PORT (lsof unavailable, or nothing bound)"
  fi
  return 0
}

command -v jq >/dev/null || die "jq is required"

log "control plane: $BASE (port $CP_PORT)"

# Preflight: surface every listener up front so a shadowing process is visible
# before the handshake even runs.
listeners=$(port_listeners)
if [[ -n $listeners ]] && (( $(printf '%s\n' "$listeners" | grep -c .) > 1 )); then
  report_listeners
fi

printf '%s  waiting for /readyz ' "$(ts)"
for _ in $(seq 1 60); do
  if curl -fsS -o /dev/null "$BASE/readyz" 2>/dev/null; then break; fi
  printf .; sleep 1
done
printf '\n'
curl -fsS -o /dev/null "$BASE/readyz" || die "control plane not ready at $BASE"

# The operator surface must accept our token. A 401/403 here almost always means
# a DIFFERENT server is answering this port — a stale dev process started without
# UPTIMEPAGE_OPERATOR__ADMIN_TOKEN. Map each outcome to an actionable message.
code=$(curl -sS -o /dev/null -w '%{http_code}' "${AUTH[@]}" "$BASE/operator/regions" || true)
case $code in
  200) log "operator surface ok (token accepted)" ;;
  401|403)
    warn "operator surface rejected the token (HTTP $code)"
    warn "token used: $(token_fp "$TOKEN") (from $(token_origin))"
    report_listeners
    # One listener => same server, so the token is the mismatch. >1 (or zero
    # under lsof) => a stale process is likely shadowing the intended server.
    if [[ $(port_listeners | grep -c .) == 1 ]]; then
      die "token mismatch: the server is up but does not accept this token. \
unset a stray OPERATOR_TOKEN (currently from $(token_origin)) so the default is \
used, or restart the control plane with a matching UPTIMEPAGE_OPERATOR__ADMIN_TOKEN"
    else
      die "stale/foreign server on port $CP_PORT. stop it (e.g. \
'pkill -f debug/uptimepage') and restart the control plane with \
UPTIMEPAGE_OPERATOR__ADMIN_TOKEN set"
    fi ;;
  404) die "operator surface is off (HTTP 404) — restart the control plane with UPTIMEPAGE_OPERATOR__ADMIN_TOKEN set" ;;
  000) die "no HTTP response from $BASE — is the control plane bound to port $CP_PORT?" ;;
  *)   die "operator surface returned unexpected HTTP $code" ;;
esac

# Delete every harness agent (any region) so the row's FK no longer pins its
# region — lets re-runs start clean and lets seed-fixtures wipe its regions.
purge_agents() {
  curl -fsS "${AUTH[@]}" "$BASE/operator/agents" \
    | jq -r '.[] | select(.name|startswith("dev-harness-")) | .id' \
    | while read -r id; do
        [[ -n $id ]] && curl -fsS -o /dev/null -X DELETE "${AUTH[@]}" "$BASE/operator/agents/$id" \
          && echo "  removed agent $id"
      done
}

if [[ $ACTION == down ]]; then
  echo "stopping agent containers…"
  "${COMPOSE[@]}" --profile agents rm -sf agent-eu agent-apac agent-us >/dev/null 2>&1 || true
  echo "deleting harness agents…"
  purge_agents
  echo "deleting harness regions…"
  for entry in "${REGIONS[@]}"; do
    region=${entry%%:*}
    curl -sS -o /dev/null -X DELETE "${AUTH[@]}" "$BASE/operator/regions/$region" || true
  done
  rm -f "$ENV_FILE"
  echo "down."
  exit 0
fi

echo "removing any prior harness agents…"
purge_agents

AGENT_CP_URL=$(resolve_agent_cp_url)
if [[ -n ${AGENT_CONTROL_PLANE_URL:-} ]]; then
  log "agents -> control plane: $AGENT_CP_URL (explicit AGENT_CONTROL_PLANE_URL)"
else
  log "agents -> control plane: $AGENT_CP_URL"
fi

: > "$ENV_FILE"
echo "# Generated by scripts/dev-regions.sh — agent tokens for the local harness." >> "$ENV_FILE"
echo "AGENT_CONTROL_PLANE_URL=$AGENT_CP_URL" >> "$ENV_FILE"

for entry in "${REGIONS[@]}"; do
  IFS=: read -r region name var city cc continent lat lon <<<"$entry"
  geo="\"city\":\"$city\",\"country_code\":\"$cc\",\"continent\":\"$continent\",\"latitude\":$lat,\"longitude\":$lon"

  # Ensure the region exists (201 created or 409 already-there are both fine).
  rc=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "${AUTH[@]}" \
        -H 'Content-Type: application/json' \
        -d "{\"id\":\"$region\",\"name\":\"$region\",$geo}" "$BASE/operator/regions")
  [[ $rc == 201 || $rc == 409 ]] || { echo "error: create region $region returned $rc" >&2; exit 1; }

  # Backfill geo on a region that already existed (POST conflict skips it).
  curl -sS -o /dev/null -X PATCH "${AUTH[@]}" -H 'Content-Type: application/json' \
        -d "{$geo}" "$BASE/operator/regions/$region" || true

  # Mint a fresh agent; the token is in the response body exactly once.
  body=$(curl -fsS -X POST "${AUTH[@]}" -H 'Content-Type: application/json' \
          -d "{\"region\":\"$region\",\"name\":\"$name\"}" "$BASE/operator/agents")
  tok=$(echo "$body" | jq -r '.token')
  [[ $tok == sm_agent_* ]] || { echo "error: no token minted for $region: $body" >&2; exit 1; }
  echo "$var=$tok" >> "$ENV_FILE"
  echo "  region $region -> agent $name minted"
done

log "tokens written to $ENV_FILE"

# Both agents run the SAME image (uptimepage:dev-agent). Build it ONCE — building
# per-service would compile the identical tag twice in parallel and the two
# builds would contend on the shared cargo target cache mount. After the first
# build the image persists (down only removes containers), so re-ups skip the
# multi-minute release compile entirely. REBUILD=1 forces a fresh build.
AGENT_IMAGE=uptimepage:dev-agent
if [[ -n ${REBUILD:-} ]] || ! docker image inspect "$AGENT_IMAGE" >/dev/null 2>&1; then
  log "building agent image $AGENT_IMAGE (first build is slow — release compile)…"
  "${COMPOSE[@]}" --env-file "$ENV_FILE" build agent-eu
else
  log "reusing existing agent image $AGENT_IMAGE (REBUILD=1 to force a rebuild)"
fi
log "starting agents…"
"${COMPOSE[@]}" --env-file "$ENV_FILE" --profile agents up -d --no-build agent-eu agent-apac agent-us

cat <<EOF

agents up (regions: eu-helsinki, apac-sg, us-east). next:
  - assign a monitor to a region: monitor form -> Regions, or
      PUT /api/v1/targets/{id}/regions  (needs an owner session — 'just dev-login')
  - watch agents:   just dev-regions-logs
  - per-region data: monitor detail view -> region selector / by-region table
  - tear down:      just dev-regions-down
EOF
