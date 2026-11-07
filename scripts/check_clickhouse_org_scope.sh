#!/usr/bin/env bash
# ClickHouse-side tenant isolation check.
#
# Every WHERE clause that filters by `target_id` or `source_id` against a
# per-tenant table must also constrain `org_id`. The `(org_id, target_id,
# timestamp)` sort key means a `target_id`-only filter still requires a
# full-org scan plus exposes other tenants' rows if a future caller drops the
# implicit org scope.

set -euo pipefail

cd "$(dirname "$0")/.."

# Find raw_string_literals targeting a tenant table that bind target_id or
# source_id but omit org_id.
hits="$(rg --pcre2 --multiline --pretty --line-number \
  -trust \
  -e '(?is)\bFROM\s+(check_results(_1m|_1h)?|flow_runs|heartbeat_pings|alert_events)\b(?:(?!\borg_id\b).)*\b(target_id|source_id)\s*=' \
  src/ || true)"

if [ -n "$hits" ]; then
  echo "ClickHouse tenant-isolation check failed: id filter without org_id." >&2
  echo >&2
  echo "$hits" >&2
  exit 1
fi
