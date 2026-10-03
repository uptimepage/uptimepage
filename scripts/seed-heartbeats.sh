#!/usr/bin/env bash
# Seed six heartbeat monitors and their ping history, so every state the
# heartbeat card can render is on screen at once:
#
#   hb-healthy     pings every 5m, declared 5m        → up, no advice
#   hb-too-tight   pings every ~83m, declared 10m     → down + "runs less often"
#   hb-too-loose   pings every 5m, declared 1h        → up + "looser than it needs"
#   hb-failed      /fail exit 137 with job output     → down + the job's own output
#   hb-running     /start 40m ago, max runtime 15m    → down + open run
#   hb-thin        three pings only                   → observed cadence, no advice
#
# Tokens are minted by the running app (KEK-sealed, so no script can forge
# one): the targets go in first and the scheduler's refresh heals the rows in.
# Everything after that waits on those rows.
#
# Matching `check_results` are written too, so the ribbon and charts above the
# card tell the same story instead of sitting empty.
#
# Env overrides:
#   SLUG          org slug to seed onto    (default: devorg)
#   PG_CONTAINER  postgres container name  (default: uptimepage-postgres-1)
#   CH_CONTAINER  clickhouse container     (default: uptimepage-clickhouse-1)
#   DAYS          history depth in days    (default: 14)
#
# Idempotent: monitors are tagged `seed-heartbeat` and both they and their
# ClickHouse rows are wiped before re-insert.
#
# Requires `just up-app` + `just dev-login` first.
set -euo pipefail

SLUG="${SLUG:-devorg}"
PG_CONTAINER="${PG_CONTAINER:-uptimepage-postgres-1}"
CH_CONTAINER="${CH_CONTAINER:-uptimepage-clickhouse-1}"
DAYS="${DAYS:-14}"

pg() { docker exec -i "$PG_CONTAINER" psql -U monitor -d monitor -v ON_ERROR_STOP=1 "$@"; }
ch() { docker exec -i "$CH_CONTAINER" clickhouse-client "$@"; }
source "$(dirname "${BASH_SOURCE[0]}")/lib/seed-purge.sh"

# A slug is only unique among live orgs, so a soft-deleted one can share it.
ORG=$(pg -tAc "SELECT id FROM organizations WHERE slug='${SLUG}' AND deleted_at IS NULL")
if [[ -z "$ORG" ]]; then
  echo "error: org '${SLUG}' missing — run 'just dev-login' (or SLUG=… just dev-login) first" >&2
  exit 1
fi

echo "==> Postgres: lift the plan gate so six heartbeats fit"
pg <<SQL
UPDATE plans SET max_targets = GREATEST(max_targets, 60)
 WHERE id = (SELECT a.plan_id FROM accounts a
              JOIN organizations o ON o.account_id = a.id
             WHERE o.id = '${ORG}');
SQL

echo "==> Postgres: (re)create the seeded heartbeat monitors"
pg <<SQL
DELETE FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['seed-heartbeat'];

INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled, tags, group_name)
VALUES
  ('${ORG}', 'Nightly backup (seeded)',
   '{"type":"heartbeat","period":300000,"grace":60000}'::jsonb,
   60, true, ARRAY['seed-heartbeat','hb-healthy'], 'Jobs'),
  ('${ORG}', 'Invoice sync (seeded)',
   '{"type":"heartbeat","period":600000,"grace":120000}'::jsonb,
   60, true, ARRAY['seed-heartbeat','hb-too-tight'], 'Jobs'),
  ('${ORG}', 'Cache warmer (seeded)',
   '{"type":"heartbeat","period":3600000,"grace":300000}'::jsonb,
   60, true, ARRAY['seed-heartbeat','hb-too-loose'], 'Jobs'),
  ('${ORG}', 'Offsite rsync (seeded)',
   '{"type":"heartbeat","period":900000,"grace":120000}'::jsonb,
   60, true, ARRAY['seed-heartbeat','hb-failed'], 'Jobs'),
  ('${ORG}', 'Warehouse ETL (seeded)',
   '{"type":"heartbeat","period":3600000,"grace":300000,"max_runtime":900000}'::jsonb,
   60, true, ARRAY['seed-heartbeat','hb-running'], 'Jobs'),
  ('${ORG}', 'Report mailer (seeded)',
   '{"type":"heartbeat","period":900000,"grace":120000}'::jsonb,
   60, true, ARRAY['seed-heartbeat','hb-thin'], 'Jobs');
SQL

echo "==> waiting for the app to mint the ping tokens"
for _ in $(seq 1 60); do
  MINTED=$(pg -tAc "SELECT count(*) FROM heartbeat_monitors hm
                      JOIN targets t ON t.id = hm.target_id
                     WHERE t.org_id = '${ORG}' AND t.tags @> ARRAY['seed-heartbeat']")
  [[ "$MINTED" == "6" ]] && break
  sleep 2
done
if [[ "${MINTED:-0}" != "6" ]]; then
  echo "error: only ${MINTED:-0}/6 tokens minted — is the app running ('just up-app')?" >&2
  exit 1
fi

id_of() { pg -tAc "SELECT id FROM targets WHERE org_id='${ORG}' AND tags @> ARRAY['$1']"; }
HEALTHY=$(id_of hb-healthy)
TIGHT=$(id_of hb-too-tight)
LOOSE=$(id_of hb-too-loose)
FAILED=$(id_of hb-failed)
RUNNING=$(id_of hb-running)
THIN=$(id_of hb-thin)

# A heartbeat is evaluated by the control plane, so its results carry that
# region; `default` is the stock id for it.
REGION="${REGION:-$(pg -tAc "SELECT id FROM regions WHERE enabled
                              ORDER BY (id <> 'default'), id LIMIT 1")}"
if [[ -z "$REGION" ]]; then
  echo "error: no enabled regions — bring the stack up first" >&2
  exit 1
fi

echo "==> Postgres: the state each monitor's last signals left behind"
pg <<SQL
-- Anchors are backdated past the seeded history so nothing looks re-armed.
UPDATE heartbeat_monitors SET armed_at = now() - interval '${DAYS} days'
 WHERE target_id IN ('${HEALTHY}','${TIGHT}','${LOOSE}','${FAILED}','${RUNNING}','${THIN}');

UPDATE heartbeat_monitors SET last_ping_at = date_trunc('second', now() - interval '2 minutes')
 WHERE target_id = '${HEALTHY}';
-- Last success is older than period + grace, which is the whole complaint.
UPDATE heartbeat_monitors SET last_ping_at = date_trunc('second', now() - interval '20 minutes')
 WHERE target_id = '${TIGHT}';
UPDATE heartbeat_monitors SET last_ping_at = date_trunc('second', now() - interval '3 minutes')
 WHERE target_id = '${LOOSE}';
UPDATE heartbeat_monitors SET last_ping_at = date_trunc('second', now() - interval '80 minutes')
 WHERE target_id = '${THIN}';

UPDATE heartbeat_monitors SET
    last_ping_at   = date_trunc('second', now() - interval '35 minutes'),
    last_start_at  = date_trunc('second', now() - interval '22 minutes'),
    last_fail_at   = date_trunc('second', now() - interval '20 minutes'),
    last_exit_code = 137
 WHERE target_id = '${FAILED}';

-- A start with no finish after it: the run is still open.
UPDATE heartbeat_monitors SET
    last_ping_at  = date_trunc('second', now() - interval '2 hours'),
    last_start_at = date_trunc('second', now() - interval '40 minutes')
 WHERE target_id = '${RUNNING}';
SQL

FAIL_AT=$(pg -tAc "SELECT extract(epoch FROM last_fail_at)::bigint FROM heartbeat_monitors WHERE target_id='${FAILED}'")
START_AT=$(pg -tAc "SELECT extract(epoch FROM last_start_at)::bigint FROM heartbeat_monitors WHERE target_id='${RUNNING}'")

echo "==> ClickHouse: wipe rows for these monitors and for any prior run"
reset_seeded_history "$ORG" seed-heartbeat

# `number` walks backwards from the monitor's last success, so 0 is the newest.
successes() { # target, gap minutes, count, offset minutes of the newest
  ch -mn <<SQL
INSERT INTO monitor.heartbeat_pings (org_id, target_id, received_at, signal)
SELECT toUUID('${ORG}'), toUUID('$1'),
       now() - toIntervalMinute($4) - toIntervalMinute(number * $2), 'success'
FROM numbers($3);
SQL
}

echo "==> ClickHouse: ping history over ${DAYS} days"
successes "$HEALTHY" 5 $(( DAYS * 24 * 60 / 5 )) 2
successes "$LOOSE"   5 $(( DAYS * 24 * 60 / 5 )) 3
successes "$THIN"    45 3 80

# Jitter around 83 minutes, so the median has to do real work. Signed, because
# `number` is unsigned and a negative offset would wrap.
ch -mn <<SQL
INSERT INTO monitor.heartbeat_pings (org_id, target_id, received_at, signal)
SELECT toUUID('${ORG}'), toUUID('${TIGHT}'),
       now() - toIntervalMinute(20)
             - toIntervalMinute(toInt64(number * 83 + number % 5) - 2), 'success'
FROM numbers($(( DAYS * 24 * 60 / 83 )));
SQL

# The failing job: a run that started, printed, and died on a signal.
ch -mn <<SQL
INSERT INTO monitor.heartbeat_pings
  (org_id, target_id, received_at, signal, exit_code, duration_ms, body)
VALUES
  (toUUID('${ORG}'), toUUID('${FAILED}'), fromUnixTimestamp(toInt64(${FAIL_AT} - 120)),
   'start', NULL, NULL, ''),
  (toUUID('${ORG}'), toUUID('${FAILED}'), fromUnixTimestamp(toInt64(${FAIL_AT})),
   'fail', 137, 120000,
   'rsync: connection unexpectedly closed (2,847,113 bytes received so far)\nrsync error: error in rsync protocol data stream (code 12) at io.c(226)\nkilled by signal 9: the container hit its memory limit');

INSERT INTO monitor.heartbeat_pings (org_id, target_id, received_at, signal)
SELECT toUUID('${ORG}'), toUUID('${FAILED}'),
       now() - toIntervalMinute(35) - toIntervalMinute(number * 15), 'success'
FROM numbers($(( DAYS * 24 * 4 )));

-- The open run: a start with nothing after it.
INSERT INTO monitor.heartbeat_pings (org_id, target_id, received_at, signal)
VALUES (toUUID('${ORG}'), toUUID('${RUNNING}'), fromUnixTimestamp(toInt64(${START_AT})), 'start');

INSERT INTO monitor.heartbeat_pings (org_id, target_id, received_at, signal)
SELECT toUUID('${ORG}'), toUUID('${RUNNING}'),
       now() - toIntervalHour(2) - toIntervalHour(number), 'success'
FROM numbers($(( DAYS * 24 )));
SQL

# One check result per evaluation the scheduler would have written: up while
# the pings were arriving, down once the monitor's own rule says so.
verdicts() { # target, minutes per evaluation, count, down-from (newest N are down), error text
  ch -mn <<SQL
INSERT INTO monitor.check_results
  (org_id, target_id, region, timestamp, status, duration_ms, error)
SELECT toUUID('${ORG}'), toUUID('$1'), '${REGION}',
       now() - toIntervalMinute(number * $2),
       if(number < $4, 'down', 'up'), 0,
       if(number < $4, '$5', NULL)
FROM numbers($3);
SQL
}

echo "==> ClickHouse: matching check_results so the charts above agree"
verdicts "$HEALTHY" 5 $(( DAYS * 24 * 12 )) 0 ''
verdicts "$LOOSE"   5 $(( DAYS * 24 * 12 )) 0 ''
verdicts "$TIGHT"   5 $(( DAYS * 24 * 12 )) 3 'no ping for 1200s, expected every 600s (+120s grace)'
verdicts "$FAILED"  5 $(( DAYS * 24 * 12 )) 4 'job reported failure (exit 137)'
verdicts "$RUNNING" 5 $(( DAYS * 24 * 12 )) 5 'job started 2400s ago and has not finished, past the 900s max runtime'
verdicts "$THIN"    5 $(( DAYS * 24 * 12 )) 12 'no ping for 4800s, expected every 900s (+120s grace)'

echo
echo "Seeded. The ping card sits under the charts on each monitor page:"
for pair in "hb-healthy:${HEALTHY}" "hb-too-tight:${TIGHT}" "hb-too-loose:${LOOSE}" \
            "hb-failed:${FAILED}" "hb-running:${RUNNING}" "hb-thin:${THIN}"; do
  name="${pair%%:*}"; id="${pair#*:}"
  pings=$(ch -q "SELECT count() FROM monitor.heartbeat_pings WHERE target_id = toUUID('${id}')")
  printf '  %-13s %5s pings  http://app.lvh.me:8080/targets/%s\n' "$name" "$pings" "$id"
done
