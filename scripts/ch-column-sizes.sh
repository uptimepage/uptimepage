#!/usr/bin/env bash
# Per-column compression benchmark for the `check_results` schema.
#
# Spins up a throwaway ClickHouse database, applies the real migrations, loads
# a synthetic-but-realistic dataset (many orgs/targets, a few regions, regular
# 60s per-series intervals, ~96% up, http + tcp mix), forces a merge, then
# dumps per-column compressed bytes / ratio / bytes-per-row. Use it to settle
# schema questions with numbers instead of guesses (e.g. "is UUID expensive?",
# "does LowCardinality region pull its weight?").
#
# The dataset is INSERT-ordered arbitrarily; ClickHouse sorts on merge by the
# table's ORDER BY, so the measured compression reflects the real on-disk
# layout, not insert order.
#
# Env overrides:
#   CH_CONTAINER  clickhouse container name   (default: uptimepage-clickhouse-1)
#   DB            throwaway database name     (default: ch_colsize_bench)
#   N_ORGS        distinct orgs               (default: 60)
#   N_TARGETS     distinct monitors           (default: 300)
#   REGIONS       region count (max 3)        (default: 3)
#   CHECKS        rows per (target,region)    (default: 2200)
#   KEEP          1 = don't drop the DB after (default: drop)
#
# Rows = N_TARGETS * REGIONS * CHECKS  (defaults ≈ 2.0M).

set -euo pipefail

cd "$(dirname "$0")/.."

CH_CONTAINER="${CH_CONTAINER:-uptimepage-clickhouse-1}"
DB="${DB:-ch_colsize_bench}"
N_ORGS="${N_ORGS:-60}"
N_TARGETS="${N_TARGETS:-300}"
REGIONS="${REGIONS:-3}"
CHECKS="${CHECKS:-2200}"
KEEP="${KEEP:-0}"

ROWS=$(( N_TARGETS * REGIONS * CHECKS ))

ch() { docker exec -i "$CH_CONTAINER" clickhouse-client "$@"; }

echo "container=$CH_CONTAINER db=$DB orgs=$N_ORGS targets=$N_TARGETS regions=$REGIONS checks=$CHECKS rows=$ROWS"

ch -q "DROP DATABASE IF EXISTS $DB"
ch -q "CREATE DATABASE $DB"

echo "applying migrations…"
ch --database="$DB" --multiquery < migrations/clickhouse/001_initial.sql
ch --database="$DB" --multiquery < migrations/clickhouse/002_check_results_1h.sql
ch --database="$DB" --multiquery < migrations/clickhouse/011_drop_check_results_1m.sql

# Per-column byte accounting only exists for WIDE parts (one file per column);
# small datasets default to COMPACT parts (all columns in one file → per-column
# sizes read as 0). Force wide so the measurement works at any row count; prod
# parts are wide anyway once they pass min_bytes_for_wide_part.
ch --database="$DB" -q "ALTER TABLE check_results
  MODIFY SETTING min_bytes_for_wide_part = 0, min_rows_for_wide_part = 0"

echo "seeding org/target uuid pools…"
ch --database="$DB" -q "CREATE TABLE _orgs ENGINE = Memory AS
  SELECT number AS idx, generateUUIDv4() AS id FROM numbers($N_ORGS)"
ch --database="$DB" -q "CREATE TABLE _targets ENGINE = Memory AS
  SELECT number AS idx, (number % $N_ORGS) AS org_idx, generateUUIDv4() AS id FROM numbers($N_TARGETS)"

echo "generating $ROWS rows…"
ch --database="$DB" -q "
INSERT INTO check_results
  (org_id, target_id, region, timestamp, ingested_at, agent_id, status,
   duration_ms, dns_ms, connect_ms, tls_ms, ttfb_ms, response_code,
   response_size, error)
SELECT
  o.id,
  t.id,
  g.region,
  g.ts,
  g.ts + toIntervalSecond(rand() % 5),
  concat('agent-', g.region),
  g.status,
  g.duration_ms,
  g.dns_ms, g.connect_ms, g.tls_ms, g.ttfb_ms,
  g.response_code, g.response_size, g.error
FROM (
  SELECT
    intDiv(number, $CHECKS)                                   AS series_idx,
    intDiv(series_idx, $REGIONS)                              AS target_idx,
    arrayElement(['eu-west','us-east','ap-south'], (series_idx % $REGIONS) + 1) AS region,
    now() - toIntervalSecond((number % $CHECKS) * 60)         AS ts,
    rand() % 1000                                            AS r,
    multiIf(r < 960, 'up', r < 980, 'degraded', r < 997, 'down', 'error') AS status,
    (target_idx % 5 = 0)                                     AS is_tcp,
    toUInt32(40 + (rand() % 300) + if(r >= 960, rand() % 1500, 0)) AS duration_ms,
    if(is_tcp, NULL, toUInt16(3  + rand() % 25))            AS dns_ms,
    if(is_tcp, NULL, toUInt16(8  + rand() % 40))            AS connect_ms,
    if(is_tcp, NULL, toUInt16(15 + rand() % 70))            AS tls_ms,
    if(is_tcp, NULL, toUInt16(15 + rand() % 180))           AS ttfb_ms,
    if(is_tcp, NULL, toUInt16(multiIf(r >= 997, 500, r >= 980, 503, 200))) AS response_code,
    if(is_tcp, NULL, toUInt32(200 + rand() % 8000))         AS response_size,
    if(r < 960, NULL, arrayElement(['timeout','connection refused','tls handshake failed','http 5xx'], (rand() % 4) + 1)) AS error,
    target_idx
  FROM numbers($ROWS)
) AS g
INNER JOIN _targets AS t ON t.idx = g.target_idx
INNER JOIN _orgs   AS o ON o.idx = t.org_idx
"

echo "merging (OPTIMIZE FINAL)…"
ch --database="$DB" -q "OPTIMIZE TABLE check_results FINAL"

echo
echo "=== per-column compression (check_results) ==="
ch --database="$DB" -q "
WITH (SELECT sum(rows) FROM system.parts
      WHERE active AND database = currentDatabase() AND table = 'check_results') AS total_rows
SELECT
  column                                                               AS name,
  any(type)                                                            AS type,
  formatReadableSize(sum(column_data_compressed_bytes))                AS comp,
  formatReadableSize(sum(column_data_uncompressed_bytes))              AS uncomp,
  round(sum(column_data_compressed_bytes) / nullIf(sum(column_data_uncompressed_bytes), 0), 3) AS ratio,
  round(sum(column_data_compressed_bytes) / nullIf(total_rows, 0), 2)  AS bytes_per_row
FROM system.parts_columns
WHERE active AND database = currentDatabase() AND table = 'check_results'
GROUP BY column
ORDER BY sum(column_data_compressed_bytes) DESC
FORMAT PrettyCompact"

echo
echo "=== table totals ==="
ch --database="$DB" -q "
WITH (SELECT sum(rows) FROM system.parts
      WHERE active AND database = currentDatabase() AND table = 'check_results') AS total_rows
SELECT
  total_rows                                                           AS rows,
  formatReadableSize(sum(column_data_compressed_bytes))                AS comp,
  formatReadableSize(sum(column_data_uncompressed_bytes))              AS uncomp,
  round(sum(column_data_compressed_bytes) / nullIf(total_rows, 0), 2)  AS bytes_per_row
FROM system.parts_columns
WHERE active AND database = currentDatabase() AND table = 'check_results'
FORMAT PrettyCompact"

if [ "$KEEP" = "1" ]; then
  echo
  echo "kept database $DB (KEEP=1)"
else
  ch -q "DROP DATABASE IF EXISTS $DB"
fi
