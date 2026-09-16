# Uptimepage — Production Deployment

This directory contains the production deployment for uptimepage:
**Caddy reverse proxy** (TLS, rate limits) in front of the Rust service,
PostgreSQL, and ClickHouse.

## What this gives you

| Concern | How it's handled |
|---|---|
| TLS certificates | Automatic via Let's Encrypt — no manual renewal |
| HTTP/2 + HTTP/3 | Enabled by default in Caddy |
| Authentication | The app's own sign-in on `app.{domain}` (UI + operator API). The edge adds no second gate |
| Public status surface | Self-host: `/status` on `app.{domain}`. SaaS: each org at `{slug}.{domain}` (apex wildcard) |
| TLS for status pages | Wildcard cert for `*.{domain}` via Let's Encrypt + Hetzner DNS-01; `app.{domain}` kept on its own per-host HTTP-01 cert |
| Public rate limit | Per-IP 60 req/min on the public surface (custom Caddy image, built automatically) |
| Auth-endpoint rate limit | Per-IP 10 req/min on `/auth/*` and `/api/v1/me`; invitation accept has its own zone at 30/min |
| Org-creation rate limit | Per-IP 3 per 24 h on `POST /api/v1/orgs` (signup-abuse speedbump) |
| Public health probes | `/healthz` and `/readyz` exposed without auth |
| Metrics scraping | Internal-only — `/metrics` returns 404 publicly |
| Security headers | HSTS, X-Frame-Options, Referrer-Policy, etc. |
| Access logging | JSON format, rotated automatically |
| Database exposure | Postgres + ClickHouse have no public ports |
| Credential storage | AES-256-GCM at rest (KEK in env) |
| ClickHouse memory | Capped via `clickhouse-config.xml`; raise for larger workloads |

## Prerequisites

- A Linux host (any cloud, any VPS, your own metal)
- Public IP with **ports 80 and 443 open**
- Docker 27+ and `docker compose` v2 (27 is where the daemon manages
  IPv6 firewall rules by default; see the AAAA note below)
- DNS zone in the **Hetzner Console** (the wildcard cert uses the Hetzner
  Cloud DNS-01 API):
  - `app.{domain}` → A/AAAA to this host (explicit record, beats the
    wildcard for the operator host)
  - `*.{domain}` → A/AAAA to this host (SaaS mode; the apex wildcard
    sends every `{slug}.{domain}` here and the app maps slug → org)

    Publishing AAAA obliges the container network to be dual-stack (the
    shipped compose is) **and** the daemon to own the v6 firewall rules,
    which is the default only from Docker 27 — before that, set
    `{"ip6tables": true}` in `/etc/docker/daemon.json`. Miss either and
    Docker SNATs every inbound IPv6 connection to the bridge gateway, so all
    v6 visitors reach the app as one internal address and the per-IP rate
    limits collapse onto it. Drop the AAAA record if you would rather stay
    v4-only.
  - A **Hetzner Cloud API token** (Read & Write) from Hetzner Console →
    your Project → Security → API Tokens, set as `HETZNER_DNS_API_TOKEN`
    in `.env`. Tokens are project-scoped, so the DNS zone must be in the
    same project (zone → Actions → Transfer to project). The legacy
    `dns.hetzner.com` DNS Console and its API were retired 2026-05.

  Self-host (single org) needs only the `app.{domain}` record and no DNS
  token — the status page is served at `https://app.{domain}/status`.

## First-time setup

### Custom Caddy image (automatic)

The stock `caddy:2-alpine` image lacks two plugins this deployment needs:

- [`caddy-dns/hetzner/v2`](https://github.com/caddy-dns/hetzner) — solves
  the ACME DNS-01 challenge for the `*.{domain}` apex wildcard
  certificate (HTTP-01 cannot validate a wildcard). v2 speaks the new
  Hetzner Console Cloud DNS API; v1 spoke the retired legacy API.
- [`caddy-ratelimit`](https://github.com/mholt/caddy-ratelimit) — per-IP
  throttle on the public status surface.

`deployment/Dockerfile.caddy` bakes both in, at pinned versions. `docker
compose up -d` builds it automatically and tags it `uptimepage-caddy:3` —
there is no manual one-time step. Bump that tag whenever the pins move: CI
validates the Caddyfile against a freshly built image, and the deploy swaps
the container when the tag no longer matches what it runs, but neither can
tell that a *reused* tag now means a different binary. To rebuild after a
Caddy or plugin bump:

```bash
# --pull: the bases are floating tags, so a cached FROM re-tags the old binary.
docker compose build --pull caddy && docker compose up -d caddy
```

### 1. Clone and enter the deployment directory

```bash
git clone <your-repo>
cd <repo>/deployment
```

### 2. Configure environment

```bash
cp .env.example .env
$EDITOR .env
```

Fill in every value. The file has inline instructions for each variable.

### 3. Generate database passwords and KEK

```bash
# Run all three at once
{
  echo "POSTGRES_PASSWORD=$(openssl rand -base64 24)"
  echo "CLICKHOUSE_PASSWORD=$(openssl rand -base64 24)"
  echo "UPTIMEPAGE_CREDENTIALS_KEK_BASE64=$(openssl rand -base64 32)"
}
```

Copy the output into `.env`.

### 4. Validate the config

```bash
docker compose config
```

This expands all env vars and prints the effective config. If any required
variable is unset, you'll see a warning.

### 5. Start the stack

```bash
docker compose up -d
docker compose logs -f caddy
```

Watch the Caddy logs. On first start it will:
1. Issue the `app.{domain}` cert via HTTP-01 (~30-60 seconds)
2. Issue the `*.{domain}` apex wildcard via the Hetzner DNS-01 challenge:
   Caddy writes a `_acme-challenge.{domain}` TXT record through the
   Hetzner API, Let's Encrypt validates it, the wildcard cert is issued —
   **allow 60-90 seconds** for this one; renewals are silent.
3. Bind to ports 80 and 443 and start proxying

When you see `serving initial configuration`, visit
`https://app.{domain}` — your browser will prompt for credentials.

Once the stack is up, see **Hardening → Firewall** below to lock the
box down at the network edge before exposing it to production traffic.

#### Verify the wildcard cert (manual test)

```bash
# Operator cert
echo | openssl s_client -connect app.example.com:443 2>/dev/null \
    | openssl x509 -noout -subject

# Wildcard cert — any slug, even one that doesn't exist as an org, must
# present a *.example.com cert (the app returns 404 for unknown slugs,
# but TLS is served by the wildcard regardless). Use a name that is NOT
# `app.` so Caddy serves the wildcard block, not the per-host operator
# cert.
echo | openssl s_client -servername anything.example.com \
    -connect anything.example.com:443 2>/dev/null \
    | openssl x509 -noout -subject
# Expect: subject=CN=*.example.com
```

If the wildcard line fails, grep the logs for the DNS-01 exchange:

```bash
docker compose logs caddy | grep -i "acme\|dns\|hetzner\|challenge"
```

Common causes: token missing or not Read & Write, the zone is not in the
token's project, the domain's authoritative DNS is not the Hetzner
Console, or `UPTIMEPAGE_DOMAIN` still set to a sub-host (it must be
the base domain, e.g. `example.com`). While
debugging, switch to the staging CA (see "Testing the TLS flow" below) so
you don't burn production rate limits.

## Hardening

### Firewall

The compose stack uses `expose:` (network-internal only) for Postgres,
ClickHouse, and the app; only Caddy publishes 80/443. The deploy
workflow fails closed if a `ports:` slip ever lands on an internal
service — the firewall below is the second layer for the manual
`docker compose up` case.

**Network-edge firewall** (blocks traffic before it reaches the host)

If the provider offers one (Hetzner Cloud Firewall, AWS Security
Groups, etc.), default-deny inbound and allow only:

| Direction | Protocol | Port | Source       | Purpose                |
|-----------|----------|------|--------------|------------------------|
| In        | TCP      | 22   | your ops IPs | SSH                    |
| In        | TCP      | 80   | Any          | HTTP (redirects → 443) |
| In        | TCP      | 443  | Any          | HTTPS                  |
| In        | UDP      | 443  | Any          | HTTP/3 (QUIC)          |

**Host firewall** (`ufw` — kicks in if the network-edge layer is
absent or misconfigured)

```bash
sudo ufw default deny incoming
sudo ufw default allow outgoing
sudo ufw allow 22/tcp comment 'SSH'
sudo ufw allow 80/tcp comment 'HTTP (Caddy)'
sudo ufw allow 443/tcp comment 'HTTPS (Caddy)'
sudo ufw allow 443/udp comment 'HTTP/3 / QUIC'
sudo ufw enable
sudo ufw status verbose
```

### Verify from outside

Run from any machine OTHER than the host (loopback bypasses both
firewalls):

```bash
# Internal ports must be filtered (no response)
nmap -Pn -p 5432,8123,9000,8080,9090,2019 app.example.com
# Expected: all six filtered/closed, none "open"
#   5432       — Postgres
#   8123,9000  — ClickHouse (HTTP + native)
#   8080,9090  — app HTTP + Prometheus metrics
#   2019       — Caddy admin API (binds 127.0.0.1; sanity check)

# Public surface must serve
curl -fsI https://app.example.com/healthz
```

### Secrets at rest

`deployment/.env` carries Postgres + ClickHouse passwords, the
credentials KEK, OAuth client secret, and Grafana write tokens. The
deploy workflow restricts it to `0600` on every run (and starts the
remote script with `umask 077` so every new file inherits owner-only
perms). Verify after first deploy:

```bash
ssh deploy@your-host 'ls -l /opt/uptimepage/deployment/.env'
# Expected: -rw------- (mode 0600), owner = deploy user
```

`clickhouse-users.xml` is intentionally world-readable — it carries no
secrets (the password is injected at runtime via `from_env`). The
`default` user still requires that runtime password, but the file
grants it network access from any IP, so the DB port-binding check +
firewall above are the primary controls — don't rely on ClickHouse
auth as the sole barrier.

## Operations

### Signing in

The stack carries no edge credential. The app gates its own routes: a page
load without a session redirects to `/login`, and `/api/v1` answers with a
401 envelope. There is nothing to configure at the proxy for it.

Sign in with the GitHub OAuth app from step 2. If you have not registered
one yet, or it is misconfigured, claim the instance from the host instead:

```bash
docker compose run --rm uptimepage_blue bootstrap-owner --email you@example.com
```

It prints a one-time sign-in URL and a full-access API token. The service is
colour-suffixed because deploys are blue/green; either colour will do, since
both run the same image against the same database.

Use the same address as the account you are recovering: a different one
inserts a *new* user with a fresh empty org and says nothing about it. The
sign-in URL appears only while magic-link is among `auth.enabled_methods`
(it is by default) and `UPTIMEPAGE_AUTH_PUBLIC_BASE_URL` is set; otherwise
you get the token alone.

Other providers use the same `UPTIMEPAGE_AUTH_<PROVIDER>_*` shape; the keys
are listed in `docker-compose.yml` on the `uptimepage_blue` service, which
`uptimepage_green` inherits.

**Signing in is a lock, not an allowlist.** OAuth sign-up provisions an org
for any identity that completes the flow, so on an internet-facing host any
GitHub account can register. `auth.open_signup = false` does *not* stop this:
it is read only on the magic-link path, never in the OAuth callback. Keep the
operator host on a private network or add an IP allowlist at the edge.

### Adding and removing people

Invite them from the app (Settings, then Team), and remove them the same
way. Nothing in `.env` or `Caddyfile` changes, and no restart is involved.

### Calling the API from scripts / CI

Mint an API token in the app and send it as a bearer token. Tokens are
scoped `resource:action`, so a CI token can be read-only.

```bash
curl -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
     https://app.example.com/api/v1/targets
```

A token bound to one org needs nothing else. A token minted for *all my
organizations* has no org to assume, so it must name one per request:

```bash
curl -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
     -H "X-Uptimepage-Org: my-org-slug" \
     https://app.example.com/api/v1/targets
```

Rotating one means minting a replacement and deleting the old token in the
app. Passwords and bcrypt hashes are not part of this stack.

### Per-IP rate limits (Caddy)

The edge enforces sixteen per-IP zones (keyed on `{remote_host}`) in
`Caddyfile`; the four most load-bearing are below, and the rest cover
`/login`, invitations, share links, heartbeat pings, on-demand checks,
channel verification, status-page subscribe, delegate connect and the three
inbound webhooks. They sit on top of the per-org / per-user budgets the app enforces from
the org's plan (see [Quotas & rate limits](../docs/quotas.md)). Per-IP is
the edge's job because behind the proxy the app sees only the proxy as the
peer; the two tiers are complementary, not redundant.

| Zone | Matches | Limit | Why |
|---|---|---|---|
| `status_path` | public status surface (`/status`, `/api/public/*`, assets) | 60 / 1 min | Cheap unauthenticated reads, bot-heavy |
| `auth_endpoints` | `/auth/*`, `/api/v1/me` | 10 / 1 min | Throttle credential stuffing / token probing |
| `oauth_register` | `POST /oauth/register` (app and wildcard hosts, one bucket) | 30 / 1 min | Open MCP client registration writes a row per call; the app drops rows nobody authorizes, this caps how fast they arrive. Public tier because MCP clients register from shared vendor egress |
| `org_creation` | `POST /api/v1/orgs` | 3 / 24 h | Signup-abuse speedbump; with email verification, mass org creation needs many real mailboxes |

These blocks already exist in the shipped `Caddyfile` — no manual step.
Excess requests get `429 Too Many Requests`. IP rotation defeats per-IP
limits by design; they are a speedbump, not a wall — the plan-driven
app-side limiter is the real budget for authenticated traffic. After editing
a zone, reload with no downtime:

```bash
docker compose exec caddy caddy reload --config /etc/caddy/Caddyfile
```

### Metrics

The in-binary Prometheus endpoint is at `http://uptimepage:9090/metrics`
on the **internal docker network**. It has no auth, so it is never
published to the host (`expose`, not `ports`) and must never be served on
the public domain. The service binds it on `0.0.0.0:9090` *inside* the
container (`UPTIMEPAGE_SERVER__METRICS_BIND` in `docker-compose.yml`)
so a sidecar container can reach it over the internal network.

#### Ship to Grafana Cloud (optional)

A Grafana Alloy sidecar can scrape that endpoint and remote-write it to
Grafana Cloud. It is **off by default** and only starts under the
`metrics` compose profile.

1. In Grafana Cloud, create an access-policy token scoped to
   `metrics:write`, and note your Prometheus remote-write URL and numeric
   instance id (Connections → Prometheus).
2. Set `GRAFANA_CLOUD_PROM_URL`, `GRAFANA_CLOUD_PROM_USER`,
   `GRAFANA_CLOUD_API_TOKEN` in `.env` (see `.env.example`). The token is
   a secret: it stays in `.env` (gitignored), is passed only via the
   container environment, and is redacted by Alloy from its own config
   endpoint. Nothing secret is written into `config.alloy`.
3. Start (or restart) the stack with the profile:

   ```bash
   docker compose --profile metrics up -d
   ```

4. Verify within ~15 s (one scrape interval): in Grafana Cloud Explore,
   `up{job="uptimepage"}` should be `1`, and the `uptimepage_*`
   series should appear. For the full view, import the dashboard against
   the Grafana Cloud Prometheus datasource — import steps and the
   datasource binding are in `dashboards/grafana/README.md`.

The sidecar is decoupled: if Alloy is stopped or fails, the service and
its `/metrics` endpoint are unaffected — only remote shipping pauses
(Alloy resumes from its on-disk WAL on restart). Scraping with your own
Prometheus instead of Alloy also works — point it at
`uptimepage:9090` on the internal network. Never expose `/metrics`
on the public domain; if you must scrape from off-host, front it with a
separate Caddy site carrying its own basic_auth and a strict IP
allowlist.

## Testing the TLS flow without rate-limit pain

Let's Encrypt production has rate limits (50 certs/week per registered
domain). While iterating, switch to staging:

In `Caddyfile`, uncomment:

```caddy
acme_ca https://acme-staging-v02.api.letsencrypt.org/directory
```

Restart Caddy. You'll get staging certs that browsers don't trust, but the
full cert flow runs. Switch back to production when ready.

If you've already issued a staging cert and want to switch to production,
**delete the caddy_data volume** to force re-issuance:

```bash
docker compose down
docker volume rm uptimepage_caddy_data
docker compose up -d
```

## Backups

Three volumes need backing up:

| Volume | Contains | Frequency |
|---|---|---|
| `caddy_data` | TLS certs and account keys (~10 MB) | Weekly |
| `postgres_data` | Target configuration | Daily |
| `clickhouse_data` | Check results history | Daily (or accept the loss) |

Postgres backup (consistent, no downtime):

```bash
docker compose exec -T postgres pg_dump -U monitor monitor | \
    gzip > "backups/postgres-$(date +%Y%m%d).sql.gz"
```

ClickHouse backup is bigger and more involved — use ClickHouse's
`BACKUP TABLE check_results TO Disk(...)` from inside the container.
See ClickHouse docs.

Caddy data is small — just copy the volume:

```bash
docker run --rm -v uptimepage_caddy_data:/source:ro \
    -v "$(pwd)/backups":/backup alpine \
    tar czf /backup/caddy-$(date +%Y%m%d).tar.gz -C /source .
```

## Upgrading

```bash
docker compose pull
docker compose up -d
```

Changing a network's own settings (subnet, `enable_ipv6`) is the one case
`up -d <service>` cannot handle: Compose stops that service, then fails to
remove the network because other containers still hold endpoints on it, and
leaves the service stopped. Apply those in a maintenance window, before any
per-service deploy runs:

```bash
# 1. Every endpoint on the network must be gone before Compose can recreate it.
#    Profiles are opt-in, so name each one in use, and detach containers from
#    other compose projects (Caddy reaches umami by name over this network).
docker network disconnect uptimepage_default umami || true

# 2. --no-build below cannot build the custom caddy image, so make sure it
#    exists first (the tag moves whenever the plugin pins do).
docker compose build --pull caddy

#    --force-recreate is required: a container that is merely restarted onto a
#    recreated network comes back with no published ports and the host's
#    resolver instead of Docker's, so the edge answers nothing.
docker compose --profile agent --profile metrics up -d --no-build --force-recreate

# 3. Reattach the foreign container — the analytics host 502s until this runs —
#    and drop a network removed from the file; `up` never prunes it.
docker network connect uptimepage_default umami
docker network rm uptimepage_probe6

# 4. That `up` started BOTH colors. Steady state is one, and the deploy wants
#    the other stopped as its rollback target.
docker compose stop uptimepage_green
```

A container deleted while an endpoint is live leaves that endpoint dangling and
the recreate keeps failing; clear it with `docker network disconnect -f
<network> <container-id from the error>`.

A Caddyfile that uses a plugin directive newer than the image on the box needs
`docker compose build caddy` first: every deploy path runs `--no-build`, so
Caddy would otherwise fail to load the config, and it is the only service
publishing 80/443. Check with `docker exec uptimepage-caddy-1 caddy validate
--config /etc/caddy/Caddyfile --adapter caddyfile` before reloading.

For Caddy config changes (Caddyfile only, no env changes), use a hot reload:

```bash
docker compose exec caddy caddy reload --config /etc/caddy/Caddyfile
```

For uptimepage upgrades, do a normal `up -d`. The new container is
distroless so Docker has no container-level healthcheck to gate on;
instead, Caddy's active `health_uri /healthz` probe (every 30s) pulls
the upstream out of rotation while it's down and back in once it
recovers. There's a brief window of 502 responses during the swap —
typically one health interval. If you need zero-downtime upgrades, run
two uptimepage replicas behind Caddy's `reverse_proxy` load
balancer (Caddy supports this with multiple upstreams).

## Troubleshooting

**Certificate fails to provision**
- DNS not propagated? `dig +short app.example.com` and
  `dig +short anything.example.com` (the apex wildcard must resolve)
- Wildcard cert stuck? Authoritative DNS must be the Hetzner Console, the
  token must be Read & Write, and the zone must be in that token's
  project — `docker compose logs caddy | grep -i hetzner`
- Ports 80/443 blocked? Test from another host: `curl -v http://app.example.com`
- Hit Let's Encrypt rate limit? Switch to staging (above) while you fix things
- Cloud firewall rules? Check security groups / firewall settings

**Sign-in bounces back to `/login`**
- OAuth callback URL mismatch? It must match `UPTIMEPAGE_AUTH_GITHUB_REDIRECT_URL` exactly.
- `UPTIMEPAGE_AUTH_PUBLIC_BASE_URL` wrong? The app builds OAuth links from it.
- Locked out entirely? `docker compose run --rm uptimepage_blue bootstrap-owner --email <the-address-on-the-account>` prints a one-time sign-in URL.

**Caddy can't reach uptimepage**
```bash
docker compose ps                                  # both running?
docker compose exec caddy wget -qO- http://uptimepage:8080/healthz
```

**uptimepage logs show "ClickHouse unreachable"**

The uptimepage image is distroless (no shell, no wget) so probe from
the caddy container, which shares the same docker network:

```bash
docker compose exec caddy wget -qO- http://clickhouse:8123/ping
```
Often a password mismatch. Re-check `CLICKHOUSE_PASSWORD` in `.env` matches
what the container actually uses (you may need to wipe the volume after
changing — Postgres and ClickHouse only honor `*_PASSWORD` env vars at
first init).

**Tuning ClickHouse memory**
The stack ships `clickhouse-config.xml` with conservative defaults suitable
for modest workloads. For larger workloads, edit
`deployment/clickhouse-config.xml` to raise `max_server_memory_usage` and
`max_memory_usage`, or delete the file entirely to use ClickHouse defaults.

## Lighthouse audit (public status page)

The public status page targets a Lighthouse accessibility score ≥ 95.
Run the audit against a live deployment — Lighthouse needs a real
HTTP origin, not an in-process router. There is no Rust harness for this;
use the `lighthouse` Node CLI on any workstation. Audit the URL your mode
serves: self-host `https://app.example.com/status`, SaaS
`https://{slug}.example.com`.

```bash
# One-shot install + run; outputs JSON + HTML reports
npx -y lighthouse@12 https://app.example.com/status \
    --only-categories=accessibility,performance,best-practices,seo \
    --output=json,html \
    --output-path=./lighthouse-status \
    --chrome-flags="--headless=new --no-sandbox" \
    --quiet
```

Capture the four category scores from `lighthouse-status.report.json`:

```bash
jq '.categories | to_entries[] | "\(.key): \(.value.score * 100)"' \
    lighthouse-status.report.json
```

Re-run after any template, CSS, or `static/js/public/*` change. The page
ships with HTMX + ~35 lines of timezone JS — under 30 KB gzipped — so
the performance category typically lands in the high 90s on a wired
connection. If accessibility drops below 95, the report's `audits`
section lists the failing rules with element selectors; the public
templates live in `templates/public/`.

## What's intentionally NOT here

This deployment is right-sized for **single-tenant, small-team operator use**:

- **No load balancer.** Caddy on one host fronts the stack. If you need
  geographic redundancy, run independent uptimepage instances per
  region.
- **No HA database.** Single Postgres, single ClickHouse. If either dies,
  the service degrades to read-only or stops accepting writes. For HA,
  switch to managed Postgres (Neon, RDS) and managed ClickHouse (ClickHouse
  Cloud, Altinity) — but you lose the single-VM simplicity.
- **No SAML / enterprise SSO.** Sign-in is OAuth (GitHub, Google,
  Microsoft, GitLab), passkeys and magic-link mail. No SCIM provisioning.
- **Per-IP throttling is targeted, not blanket.** Fifteen named zones cover
  the surfaces worth bounding (see the table above); everything else falls
  through unthrottled. To bound another path, add its own `handle` block
  with a matcher, a `rate_limit` and `import app_upstream` — do not put
  `rate_limit` at site scope or inside the shared snippet, since either
  applies it to `/healthz` and the public status surface too. Zone names are
  global to the Caddy process, so pick a fresh one.
- **No WAF / DDoS protection.** Front this with Cloudflare (free tier) if
  you need that.

These are deliberate omissions, each documented as a known gap.

## File reference

| File | Purpose |
|---|---|
| `Caddyfile` | Caddy v2 reverse proxy config (operator + wildcard status) |
| `Dockerfile.caddy` | Custom Caddy image (Hetzner DNS-01 + rate-limit plugins) |
| `docker-compose.yml` | Service definitions for the full stack |
| `config.alloy` | Grafana Alloy config for the optional `metrics` profile (scrape + remote-write to Grafana Cloud) |
| `.env.example` | Template for `.env` (commit this; never commit `.env`) |
| `README.md` | This file |
