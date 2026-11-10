#!/usr/bin/env bash
# uptimepage backup — PostgreSQL + ClickHouse + Caddy certs, local disk.
#
#   backup.sh daily    pg_dump + ClickHouse dump  (cron: nightly)
#   backup.sh weekly   tar of the caddy_data volume (certs/keys)
#
# Backups land under $BACKUP_ROOT (default /opt/uptimepage/backups),
# mode 0700 (they contain a full DB dump — treat as secret). Retention is
# pruned at the end of each run. Designed to run as the `deploy` user from
# the compose project directory; it shells into the running containers, so
# the stack must be up.
#
# Restore is documented in BACKUP.md (same directory).
set -euo pipefail

ACTION="${1:-}"
COMPOSE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BACKUP_ROOT="${BACKUP_ROOT:-/opt/uptimepage/backups}"
PG_KEEP_DAYS="${PG_KEEP_DAYS:-30}"
CH_KEEP_DAYS="${CH_KEEP_DAYS:-3}"
CADDY_KEEP_DAYS="${CADDY_KEEP_DAYS:-28}"
PROJECT="uptimepage"            # compose `name:` — used for the volume name
TS="$(date -u +%Y%m%dT%H%M%SZ)"
LOG="$BACKUP_ROOT/backup.log"

log() { echo "$(date -u +%FT%TZ) [$ACTION] $*" | tee -a "$LOG" >&2; }
die() { log "ERROR: $*"; exit 1; }

# A key's value from the compose .env, read without echoing; empty when the
# file or the key is missing, so callers report it instead of errexit firing.
env_value() { grep -E "^$1=" "$COMPOSE_DIR/.env" 2>/dev/null | head -n1 | cut -d= -f2- || true; }

# The dump in progress lives under a .partial name until publish() renames it.
# A failed run removes it, so a short file never passes for the newest backup.
PARTIAL=""

# Optional heartbeat monitor: `start` when the run begins, then its exit code
# (0 = success, any other fails the monitor at once) with this action's last
# log lines as the ping body. The URL comes from the environment (set but empty
# silences a run), else from .env as BACKUP_HEARTBEAT_DAILY_URL or _WEEKLY_URL.
BACKUP_HEARTBEAT_URL="${BACKUP_HEARTBEAT_URL-$(env_value "BACKUP_HEARTBEAT_${ACTION^^}_URL")}"
BACKUP_HEARTBEAT_URL="${BACKUP_HEARTBEAT_URL%/}"
LOG_START=0

heartbeat() { # start | exit code
  [ -n "$BACKUP_HEARTBEAT_URL" ] || return 0
  local body=""
  [ "$1" = start ] \
    || body="$(tail -n "+$((LOG_START + 1))" "$LOG" 2>/dev/null | grep -F "[$ACTION]" | tail -n 20 || true)"
  printf '%s\n' "$body" | curl -fsS -m 10 --retry 3 --retry-connrefused -o /dev/null \
    --data-binary @- "$BACKUP_HEARTBEAT_URL/$1" || log "heartbeat ping failed"
}

# Without errexit: a failed cleanup or log write (a full disk) must not skip
# the ping that reports the failure.
on_exit() {
  local rc=$?
  set +e
  [ -z "$PARTIAL" ] || rm -rf -- "$PARTIAL"
  [ "$rc" -eq 0 ] || log "exited with status $rc"
  heartbeat "$rc"
}
trap on_exit EXIT

# Yield the CPU to the app and the edge. Reaches only this shell's own
# children; a process started by `docker exec` or `docker run` gets its
# priority from the container, so those that do real work call nice there.
# No ionice: the disk queue on the hosted box has no scheduler to honor it.
renice -n 19 -p $$ >/dev/null

cd "$COMPOSE_DIR"
mkdir -p "$BACKUP_ROOT"/{postgres,clickhouse,caddy}
chmod 700 "$BACKUP_ROOT" "$BACKUP_ROOT"/{postgres,clickhouse,caddy}
LOG_START="$(wc -l 2>/dev/null < "$LOG" || echo 0)"
heartbeat start

publish() { # tmp, out
  chmod 600 "$1"
  mv "$1" "$2"
  PARTIAL=""
}

# CLICKHOUSE_PASSWORD authenticates clickhouse-client. POSTGRES uses the
# in-container local socket as the `monitor` superuser, so needs no password.
ch_password() { env_value CLICKHOUSE_PASSWORD; }

backup_postgres() {
  local out="$BACKUP_ROOT/postgres/monitor-$TS.sql.gz"
  local tmp="$out.partial"
  PARTIAL="$tmp"
  log "pg_dump → $(basename "$out")"
  docker compose exec -T postgres pg_dump -U monitor --no-owner --no-privileges monitor \
    | gzip -c > "$tmp"
  # grep without -q reads to the end, so tail never takes a SIGPIPE under pipefail.
  gzip -cd "$tmp" | tail -n 8 | grep 'PostgreSQL database dump complete' >/dev/null \
    || die "pg dump has no completion marker"
  publish "$tmp" "$out"
  log "pg_dump ok ($(du -h "$out" | cut -f1))"
}

backup_clickhouse() {
  local out="$BACKUP_ROOT/clickhouse/monitor-$TS.tgz"
  local work="$BACKUP_ROOT/clickhouse/monitor-$TS.partial"
  local dir="$work/monitor-$TS"
  local pw; pw="$(ch_password)"
  [ -n "$pw" ] || die "CLICKHOUSE_PASSWORD not found in .env"
  PARTIAL="$work"
  mkdir -p "$dir"; chmod 700 "$work" "$dir"
  # One schema file + one gzipped Native data file per table. Native is the
  # most faithful round-trip format; restore replays CREATE then INSERT.
  # gzip -1: about a third of the CPU of the default level for ~15% more bytes,
  # and this is the bulk of the nightly run.
  # `mapfile` (not a `while read` loop): the in-loop `docker compose exec -T`
  # inherits the loop's stdin and would swallow the remaining table list,
  # silently backing up only the first table.
  local tables
  mapfile -t tables < <(docker compose exec -T clickhouse \
    clickhouse-client --password "$pw" \
    --query "SELECT name FROM system.tables WHERE database='monitor' AND engine NOT LIKE '%View%'")
  [ "${#tables[@]}" -gt 0 ] || die "no tables found in clickhouse db 'monitor'"
  local t
  for t in "${tables[@]}"; do
    [ -n "$t" ] || continue
    docker compose exec -T clickhouse clickhouse-client --password "$pw" \
      --query "SHOW CREATE TABLE monitor.\`$t\`" > "$dir/$t.schema.sql"
    docker compose exec -T clickhouse nice -n 19 clickhouse-client --password "$pw" \
      --query "SELECT * FROM monitor.\`$t\` FORMAT Native" | gzip -1 -c > "$dir/$t.native.gz"
    log "clickhouse table $t ok ($(du -h "$dir/$t.native.gz" | cut -f1))"
  done
  tar -C "$work" -czf "$work/$(basename "$out")" "$(basename "$dir")"
  publish "$work/$(basename "$out")" "$out"
  rm -rf "$work"
  log "clickhouse ok ($(du -h "$out" | cut -f1))"
}

backup_caddy() {
  local vol="${PROJECT}_caddy_data"
  local out="$BACKUP_ROOT/caddy/caddy-data-$TS.tgz"
  local tmp="$out.partial"
  docker volume inspect "$vol" >/dev/null 2>&1 || die "volume $vol not found"
  PARTIAL="$tmp"
  log "tar caddy volume $vol → $(basename "$out")"
  # Throwaway alpine reads the named volume and streams the archive; the host
  # shell writes the file, so it belongs to the host user, not container root.
  docker run --rm -v "$vol":/data:ro alpine tar -C /data -czf - . > "$tmp"
  tar -tzf "$tmp" >/dev/null || die "caddy archive is unreadable"
  publish "$tmp" "$out"
  log "caddy ok ($(du -h "$out" | cut -f1))"
}

prune() { # dir, days
  {
    find "$1" -type f \( -name '*.gz' -o -name '*.tgz' \) ! -path '*.partial/*' -mtime "+$2" -print -delete
    # A killed run never reaches its EXIT trap; the next day's run sweeps what it left.
    find "$1" -mindepth 1 -maxdepth 1 -name '*.partial' -mmin +720 -print -exec rm -rf {} +
  } | while read -r f; do
    log "pruned $(basename "$f")"
  done
}

case "$ACTION" in
  daily)
    backup_postgres
    backup_clickhouse
    prune "$BACKUP_ROOT/postgres"   "$PG_KEEP_DAYS"
    prune "$BACKUP_ROOT/clickhouse" "$CH_KEEP_DAYS"
    log "daily complete"
    ;;
  weekly)
    backup_caddy
    prune "$BACKUP_ROOT/caddy" "$CADDY_KEEP_DAYS"
    log "weekly complete"
    ;;
  *)
    echo "usage: $0 {daily|weekly}" >&2
    exit 2
    ;;
esac
