#!/usr/bin/env bash
# Metric-name drift gate. Fails (non-zero) if:
#   1. a dashboard panel references a uptimepage_* name absent from the binary
#   2. a uptimepage_* name in docs/metrics.md is absent from the binary
#   3. a metric name registered in src/metric_names.rs or src/observability/metrics.rs is missing
#      from the docs/metrics.md series table
#   4. an alert rule queries a uptimepage_* name absent from the binary. A
#      misspelled name returns no series, which the rules read as no_data =
#      OK — a rule that looks healthy and can never fire.
# Run from anywhere; paths resolve relative to the repo root.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

dash_dir="$root/terraform/dashboards"
alerts_tf="$root/terraform/alerts.tf"
metrics_rs="$root/src/observability/metrics.rs"
names_rs="$root/src/metric_names.rs"
metrics_md="$root/docs/metrics.md"

fail=0
emit() { echo "DRIFT: $*" >&2; fail=1; }

# Names the binary actually knows: quoted "uptimepage_*" literals in
# metric_names.rs and metrics.rs (the consts, describe_* calls, and build_info). Require a
# trailing letter so a bucket-matcher prefix like "uptimepage_check_" is not
# mistaken for a metric name.
mapfile -t code_names < <(grep -ohE '"uptimepage_[a-z0-9_]*[a-z0-9]"' "$metrics_rs" "$names_rs" | tr -d '"' | sort -u)

contains() { local n="$1"; shift; local x; for x in "$@"; do [[ "$x" == "$n" ]] && return 0; done; return 1; }

# Histogram/summary exposition appends _bucket/_sum/_count to the base name;
# strip those so a panel querying a _bucket series maps back to the registered
# metric. No registered name ends in these, so the strip is unambiguous.
strip_suffix='s/_(bucket|sum|count)$//'
# Drop prefix/glob references (a real metric name never ends in `_`), e.g. a
# `uptimepage_check_*_ms` family mention in prose or a bucket-matcher prefix.
drop_globs='/_$/d'

# 1. dashboards -> binary (every board JSON in the dir)
while read -r n; do
  contains "$n" "${code_names[@]}" || emit "panel uses '$n' — not registered in src/metric_names.rs"
done < <(grep -ohE 'uptimepage_[a-z0-9_]+' "$dash_dir"/*.json | sed -E -e "$strip_suffix" -e "$drop_globs" | sort -u)

# 2. docs -> binary
while read -r n; do
  contains "$n" "${code_names[@]}" || emit "docs/metrics.md lists '$n' — not registered in src/metric_names.rs"
done < <(grep -oE 'uptimepage_[a-z0-9_]+' "$metrics_md" | sed -E -e "$strip_suffix" -e "$drop_globs" | sort -u)

# 3. binary -> docs
for n in "${code_names[@]}"; do
  grep -q "$n" "$metrics_md" || emit "metric '$n' registered in code but missing from docs/metrics.md"
done

# 4. alert rules -> binary. Non-uptimepage series (node_exporter and friends)
# are out of scope: the grep only matches our own prefix.
while read -r n; do
  contains "$n" "${code_names[@]}" || emit "alert rule queries '$n' — not registered in src/metric_names.rs"
done < <(grep -ohE 'uptimepage_[a-z0-9_]+' "$alerts_tf" | sed -E -e "$strip_suffix" -e "$drop_globs" | sort -u)

if [[ $fail -eq 0 ]]; then
  echo "metric names: dashboard <-> alerts <-> code <-> docs all consistent"
fi
exit $fail
