#!/usr/bin/env bash
# Seed one browser-flow monitor and 14 days of run history into a running local
# stack, so the monitor page's flow panel can be looked at without waiting for
# a real engine to accumulate it.
#
# The history is shaped to put every rendered state on screen at once:
#   * a passing 5-step login, every 15 minutes, with the `assert_url` step
#     degrading from ~180 ms to ~2.2 s across the window (the per-step trend)
#   * a failure cluster ~3 h ago  — trace + page snapshot, inside its window
#   * a failure cluster ~9 d ago  — trace kept, page snapshot expired by TTL
#   * one whole-run budget overrun (Error, names the step it was on)
#   * one engine failure (Error, no trace at all)
#
# Matching `check_results` rows are written too, so the ribbon, KPI cards and
# charts above the panel show the same story rather than sitting empty.
#
# The 9-day cluster's snapshot is dropped by the table's own per-column TTL —
# this script runs OPTIMIZE to force the merge, so what you see is the real
# retention behaviour and not a fixture pretending to be it.
#
# Idempotent: the monitor is tagged `seed-flow` and both it and its ClickHouse
# rows are wiped before re-insert.
#
# Env overrides:
#   SLUG          org slug to seed onto    (default: devorg)
#   PG_CONTAINER  postgres container name  (default: uptimepage-postgres-1)
#   CH_CONTAINER  clickhouse container     (default: uptimepage-clickhouse-1)
#   DAYS          history depth in days    (default: 14)
#
# Requires `just up-app` + `just dev-login` first (the org must already exist).
set -euo pipefail

SLUG="${SLUG:-devorg}"
PG_CONTAINER="${PG_CONTAINER:-uptimepage-postgres-1}"
CH_CONTAINER="${CH_CONTAINER:-uptimepage-clickhouse-1}"
DAYS="${DAYS:-14}"

pg() { docker exec -i "$PG_CONTAINER" psql -U monitor -d monitor -v ON_ERROR_STOP=1 "$@"; }
ch() { docker exec -i "$CH_CONTAINER" clickhouse-client "$@"; }
source "$(dirname "${BASH_SOURCE[0]}")/lib/seed-purge.sh"

if ! pg -tAc "SELECT 1 FROM organizations WHERE slug='${SLUG}'" | grep -q 1; then
  echo "error: org '${SLUG}' missing — run 'just dev-login' (or SLUG=… just dev-login) first" >&2
  exit 1
fi
ORG=$(pg -tAc "SELECT id FROM organizations WHERE slug='${SLUG}'")

# Runs every 15 minutes; `number` walks backwards from now, so 0 is the newest.
STEP_MIN=15
RUNS=$(( DAYS * 24 * 60 / STEP_MIN ))
PER_DAY=$(( 24 * 60 / STEP_MIN ))
FAIL_RECENT_FROM=12                      # ~3 h ago
FAIL_RECENT_TO=17
FAIL_OLD_FROM=$(( 9 * PER_DAY ))         # ~9 d ago, past the evidence window
FAIL_OLD_TO=$(( FAIL_OLD_FROM + 5 ))
BUDGET_AT=$(( 5 * PER_DAY ))             # ~5 d ago
ENGINE_AT=$(( 2 * PER_DAY ))             # ~2 d ago

echo "==> Postgres: lift the plan gate so the monitor is creatable and editable"
pg <<SQL
UPDATE plans SET max_flow_checks = GREATEST(max_flow_checks, 5),
                 max_flow_steps  = GREATEST(max_flow_steps, 30)
 WHERE id = (SELECT a.plan_id FROM accounts a
              JOIN organizations o ON o.account_id = a.id
             WHERE o.slug = '${SLUG}');
SQL

echo "==> Postgres: (re)create the seeded flow monitor"
pg <<SQL
DELETE FROM targets
 WHERE org_id = '${ORG}' AND tags @> ARRAY['seed-flow'];

INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled, tags, group_name)
VALUES (
  '${ORG}',
  'Login flow (seeded)',
  '{"type":"flow",
    "start_url":"https://the-internet.herokuapp.com/login",
    "steps":[
      {"op":"fill","selector":"#username","value":"tomsmith"},
      {"op":"fill","selector":"#password","value":"SuperSecretPassword!"},
      {"op":"click","selector":"#login > button"},
      {"op":"assert_url","contains":"/secure"},
      {"op":"assert_text","contains":"secure area"}
    ],
    "timeout":30000,
    "step_timeout":10000,
    "verify_tls":true}'::jsonb,
  $(( STEP_MIN * 60 )), true, ARRAY['seed-flow'], 'Journeys'
);

-- Assigned to whatever regions the running stack actually has, same as any
-- other monitor; the seed invents none.
INSERT INTO target_regions (target_id, region)
SELECT t.id, r.id FROM targets t CROSS JOIN regions r
 WHERE t.org_id = '${ORG}' AND t.tags @> ARRAY['seed-flow'] AND r.enabled
ON CONFLICT DO NOTHING;
SQL

TARGET=$(pg -tAc "SELECT id FROM targets WHERE org_id='${ORG}' AND tags @> ARRAY['seed-flow']")
# A flow only runs where an engine exists, so the history is attributed to a
# flow-capable region when the stack has one.
REGION=$(pg -tAc "SELECT tr.region FROM target_regions tr
                    LEFT JOIN agents a ON a.region = tr.region AND a.enabled AND a.flow_capable
                   WHERE tr.target_id = '${TARGET}'
                   ORDER BY (a.id IS NULL), tr.region LIMIT 1")
if [[ -z "$REGION" ]]; then
  echo "error: no enabled regions — bring the stack up (and optionally 'just dev-regions') first" >&2
  exit 1
fi
echo "    monitor ${TARGET} in ${REGION}"

echo "==> ClickHouse: wipe rows for this monitor and for any prior run"
reset_seeded_history "$ORG" seed-flow

# One expression, reused by every insert below: the step timings, with
# `assert_url` climbing as `number` shrinks (i.e. as the run gets more recent).
ASSERT_MS="toUInt32(180 + intDiv((${RUNS} - number) * 2000, ${RUNS}))"
STEP_MS="[toUInt32(10 + number % 5), toUInt32(8 + number % 4), toUInt32(38 + number % 9), ${ASSERT_MS}, toUInt32(110 + number % 30)]"
ALL_OPS="['fill','fill','click','assert_url','assert_text']"
TS="now() - toIntervalMinute(number * ${STEP_MIN})"
SKIP="number NOT BETWEEN ${FAIL_RECENT_FROM} AND ${FAIL_RECENT_TO} \
  AND number NOT BETWEEN ${FAIL_OLD_FROM} AND ${FAIL_OLD_TO} \
  AND number != ${BUDGET_AT} AND number != ${ENGINE_AT}"

echo "==> ClickHouse: ${RUNS} runs over ${DAYS} days"
ch -mn <<SQL
-- Passing runs: full trace, no page (there was nothing to explain).
INSERT INTO monitor.flow_runs
  (org_id, target_id, region, timestamp, status, duration_ms, stopped_step, error,
   step_op, step_outcome, step_ms)
SELECT toUUID('${ORG}'), toUUID('${TARGET}'), '${REGION}', ${TS},
       'up',
       arraySum(${STEP_MS}) + 900 + (number % 250),
       NULL, '',
       ${ALL_OPS},
       ['passed','passed','passed','passed','passed'],
       ${STEP_MS}
FROM numbers(${RUNS})
WHERE ${SKIP};

-- Both failure clusters: the login is rejected, so assert_url never sees
-- /secure and the step after it is never reached.
INSERT INTO monitor.flow_runs
  (org_id, target_id, region, timestamp, status, duration_ms, stopped_step, error,
   step_op, step_outcome, step_ms,
   final_url, title, text_snippet, console_level, console_text)
SELECT toUUID('${ORG}'), toUUID('${TARGET}'), '${REGION}', ${TS},
       'down', 12400 + (number % 300), 3,
       'step 4/5 assert_url: url does not contain "/secure"',
       ${ALL_OPS},
       ['passed','passed','passed','failed','skipped'],
       [toUInt32(11), toUInt32(9), toUInt32(42), toUInt32(10000), toUInt32(0)],
       'https://the-internet.herokuapp.com/login',
       'The Internet',
       'Your password is invalid! Username Password Login Powered by Elemental Selenium',
       ['error'],
       ['Failed to load resource: the server responded with a status of 401']
FROM numbers(${RUNS})
WHERE number BETWEEN ${FAIL_RECENT_FROM} AND ${FAIL_RECENT_TO}
   OR number BETWEEN ${FAIL_OLD_FROM} AND ${FAIL_OLD_TO};

-- Budget overrun: reported Error, not Down — the target never got to answer.
INSERT INTO monitor.flow_runs
  (org_id, target_id, region, timestamp, status, duration_ms, stopped_step, error,
   step_op, step_outcome, step_ms,
   final_url, title, text_snippet)
SELECT toUUID('${ORG}'), toUUID('${TARGET}'), '${REGION}', ${TS},
       'error', 30004, 3, 'step 4/5 assert_url: run budget spent',
       ${ALL_OPS},
       ['passed','passed','passed','failed','skipped'],
       [toUInt32(12), toUInt32(9), toUInt32(41), toUInt32(29800), toUInt32(0)],
       'https://the-internet.herokuapp.com/login',
       'The Internet',
       'Login Username Password'
FROM numbers(${RUNS}) WHERE number = ${BUDGET_AT};

-- Engine failure: no trace at all, because the run never reached the steps.
INSERT INTO monitor.flow_runs
  (org_id, target_id, region, timestamp, status, duration_ms, stopped_step, error,
   step_op, step_outcome, step_ms)
SELECT toUUID('${ORG}'), toUUID('${TARGET}'), '${REGION}', ${TS},
       'error', 5231, NULL,
       'engine did not start after retries: lightpanda exited on startup (exit status: 1)',
       [], [], []
FROM numbers(${RUNS}) WHERE number = ${ENGINE_AT};
SQL

echo "==> ClickHouse: matching check_results so the charts above agree"
ch -mn <<SQL
INSERT INTO monitor.check_results
  (org_id, target_id, region, timestamp, status, duration_ms, error)
SELECT toUUID('${ORG}'), toUUID('${TARGET}'), '${REGION}', timestamp, status, duration_ms,
       if(error = '', NULL, error)
FROM monitor.flow_runs
WHERE org_id = toUUID('${ORG}') AND target_id = toUUID('${TARGET}');
SQL

echo "==> ClickHouse: force the TTL merge so the old cluster really loses its page"
ch -q "OPTIMIZE TABLE monitor.flow_runs FINAL"

ch -q "SELECT concat('    ', toString(count()), ' runs · ',
                     toString(countIf(status = 'up')), ' up · ',
                     toString(countIf(status = 'down')), ' down · ',
                     toString(countIf(status = 'error')), ' error · ',
                     toString(countIf(length(text_snippet) > 0)), ' still holding a page')
       FROM monitor.flow_runs WHERE target_id = toUUID('${TARGET}')"

echo
echo "Seeded. Open the monitor and switch the range pills:"
echo "  http://app.lvh.me:8080/targets/${TARGET}"
