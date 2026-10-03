# Sourced by the seed scripts, which define `pg` and `ch`.
#
# Seeds recreate their monitors with fresh ids, so the old ids' ClickHouse rows,
# rollups included, outlive them. After recreating, this deletes the org's rows
# outside its live monitors not tagged TAG: prior runs' orphans (and those of
# monitors deleted in the UI) plus whatever the new seeded monitors caught.
reset_seeded_history() {
  local org="$1" tag="$2" keep tables table where sql=""
  keep=$(pg -tAc "SELECT string_agg(format('toUUID(%L)', id), ',') FROM targets
                   WHERE org_id = '${org}' AND NOT tags @> ARRAY['${tag}']")
  where="org_id = toUUID('${org}')"
  if [[ -n "$keep" ]]; then
    where+=" AND target_id NOT IN (${keep})"
  fi
  tables=$(ch -q "SELECT table FROM system.columns
                   WHERE database = 'monitor' AND name IN ('org_id', 'target_id')
                     AND table NOT LIKE '.inner%'
                   GROUP BY table HAVING count() = 2")
  if [[ -z "$tables" ]]; then
    echo "error: found no ClickHouse tables to reset in database 'monitor'" >&2
    return 1
  fi
  for table in $tables; do
    sql+="ALTER TABLE monitor.${table} DELETE WHERE ${where} SETTINGS mutations_sync = 1;"$'\n'
  done
  ch -mn <<<"$sql"
}
