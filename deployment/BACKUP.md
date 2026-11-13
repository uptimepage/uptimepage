# Backups & restore

`backup.sh` dumps PostgreSQL + ClickHouse nightly and the Caddy cert volume
weekly, to local disk under `/opt/uptimepage/backups` (mode 0700 — the
dumps contain everything; treat them as secret).

| What | When | Retention | File |
|------|------|-----------|------|
| PostgreSQL (`pg_dump`, gzip) | daily | 30 copies | `postgres/monitor-<ts>.sql.gz` |
| ClickHouse (schema + Native data per table, tarred) | daily | 3 copies | `clickhouse/monitor-<ts>.tgz` |
| Caddy data volume (certs/keys) | weekly | 4 copies | `caddy/caddy-data-<ts>.tgz` |

PostgreSQL is the irreplaceable data (orgs, users, targets, sessions).
ClickHouse is check-result history — large but self-healing as monitors keep
running. Caddy certs re-issue automatically, but the volume backup avoids
hitting Let's Encrypt rate limits on a full rebuild.

Each run deletes all but the newest copies of its kind, so a night that failed never costs a good copy. A manual `backup.sh` run counts as a copy. Files under other names are never pruned, so a dump saved by hand stays until you delete it. Set `PG_KEEP`, `CH_KEEP` or `CADDY_KEEP` in the cron environment to change how many are kept.

## Cron (deploy user)

```
30 3 * * *  /opt/uptimepage/deployment/backup.sh daily   >/dev/null 2>&1
0  4 * * 0  /opt/uptimepage/deployment/backup.sh weekly  >/dev/null 2>&1
```

Activity is appended to `/opt/uptimepage/backups/backup.log`.

To be alerted when a run fails or stops running, create a heartbeat monitor per job and put its ping URL in `.env` as `BACKUP_HEARTBEAT_DAILY_URL` and `BACKUP_HEARTBEAT_WEEKLY_URL`; the deploy workflow writes them from the GitHub secrets of the same names. A `BACKUP_HEARTBEAT_URL=https://…/ping/<token>` in front of a command overrides `.env` for that run, and an empty `BACKUP_HEARTBEAT_URL=` keeps a manual run from pinging. Deleting a secret does not remove its key from `.env`; clear the key there to stop the pings. Each run pings `/start` when it begins and its exit code when it ends (`/0` passes, anything else fails the monitor at once), with up to 20 of the lines this run logged as the ping body. A run that never reports trips the monitor once its period and grace are up, and a max run time on the monitor catches one that hangs. Put the monitor on a different instance when you can: one on the instance being backed up goes quiet with it, and a ping sent while that instance restarts is lost.

## Restore

All commands run from `/opt/uptimepage/deployment` with the stack up.

### PostgreSQL

Hold deploys (no pushes to `main`) until the restore is done: a deploy that finds no app running starts one, and its migrations then race the restore.

The app runs as one of two compose services, `uptimepage_blue` or `uptimepage_green`. The other one is stopped but still holds the previous release, so the block stops and restarts only the running color, and falls back to `.active_color` (the last live color) when neither is up. Paste it as one block: each step runs only if the one before it succeeded, so a failed stop or drop never lets the dump replay over live data.

```bash
dump=/opt/uptimepage/backups/postgres/monitor-<ts>.sql.gz
active=$(docker compose ps --services --status running | grep -E '^uptimepage_(blue|green)$') \
  || active="uptimepage_$(cat .active_color)"
gunzip -c "$dump" | tail -n 8 | grep -q 'PostgreSQL database dump complete' \
  && docker compose stop "$active" \
  && docker compose exec -T postgres psql -X -U monitor -d postgres -v ON_ERROR_STOP=1 \
       -c 'DROP DATABASE IF EXISTS monitor WITH (FORCE);' -c 'CREATE DATABASE monitor;' \
  && gunzip -c "$dump" | docker compose exec -T postgres \
       psql -X -q -U monitor -d monitor -v ON_ERROR_STOP=1 \
  && docker compose start "$active"
```

The first step refuses a dump without pg_dump's closing line: a `pg_dump` that died mid-run still leaves a well-formed `.gz`, just a short one. `WITH (FORCE)` ends other sessions (a cron `pg_dump`, a tunnelled `psql`) that would otherwise block the drop. `DROP` and `CREATE` sit in separate `-c` flags because psql runs a single `-c` string as one transaction, and `DROP DATABASE` refuses to run inside one. The restore stops at the first error and leaves the app stopped; rerun the block with an older dump, which drops the partial database first.

`start` brings back the exact container that was stopped. If that container no longer exists (after a `docker compose down`), `start` silently does nothing: use `docker compose up -d --no-build "$active"` instead, after checking that `UPTIMEPAGE_IMAGE` in `.env` names the release you want, since a failed deploy leaves its own image there.

### ClickHouse

```bash
tmp=$(mktemp -d)
tar -C "$tmp" -xzf /opt/uptimepage/backups/clickhouse/monitor-<ts>.tgz
cd "$tmp"/monitor-<ts>
pw=$(grep -E '^CLICKHOUSE_PASSWORD=' /opt/uptimepage/deployment/.env | cut -d= -f2-)
for s in *.schema.sql; do
  t=${s%.schema.sql}
  # .schema.sql holds the original CREATE TABLE; apply it, then load data.
  docker compose -f /opt/uptimepage/deployment/docker-compose.yml exec -T clickhouse \
    clickhouse-client --password "$pw" --query "$(cat "$s")"
  gunzip -c "$t.native.gz" | docker compose -f /opt/uptimepage/deployment/docker-compose.yml \
    exec -T clickhouse clickhouse-client --password "$pw" \
    --query "INSERT INTO monitor.\`$t\` FORMAT Native"
done
```

(The app re-applies ClickHouse migrations on boot, so an empty DB also
self-heals its schema — the data load is the part that matters.)

### Caddy certs

```bash
docker compose stop caddy
docker run --rm -v uptimepage_caddy_data:/data -v \
  /opt/uptimepage/backups/caddy:/backup alpine \
  sh -c 'cd /data && tar xzf /backup/caddy-data-<ts>.tgz'
docker compose start caddy
```

## Off-site (not configured)

Backups are local only — they survive a bad deploy or volume corruption, not
total server loss. Off-site replication (Hetzner Storage Box / S3) is a
follow-up; until then, periodically copy `postgres/` off the box by hand.
