# Deployment

Running on Kubernetes instead? See [Kubernetes](kubernetes.md) for the Helm charts.

## Production deployment with Caddy

For real-world operation, use the production stack under `deployment/` in the repo. It puts a Caddy reverse proxy in front of the Rust service with:

- Automatic TLS via Let's Encrypt (HTTP/2 and HTTP/3 on by default)
- The app's own sign-in guarding the UI and API; the edge adds no second credential
- Postgres and ClickHouse on the internal docker network — **no published ports**
- ClickHouse memory-capped at ~2 GB (see `deployment/clickhouse-config.xml`)

Setup:

```bash
cd deployment
cp .env.example .env
$EDITOR .env            # set domain, ACME email, OAuth app, DB passwords, KEK
docker compose up -d
```

`deployment/README.md` is the authoritative source for setup, signing in, backups, and troubleshooting.

### Authentication boundary

The Rust service ships an in-binary auth stack (GitHub, Google, Microsoft and GitLab OAuth, opaque API tokens, and magic-link sign-in, all enabled by default and each individually configurable). The native auth is the boundary; a basic-auth layer in front of Caddy would double-prompt. Single-tenant deploys behave the same way. Note that OAuth sign-up provisions an org for any identity that completes the flow, so an internet-facing instance that should not accept strangers needs a network or edge restriction of its own.

`/healthz` and `/readyz` are intentionally exposed without auth so
uptime probes, load balancers, and orchestrators can hit them.
`/metrics` on the public domain returns 404 — scrape it on the internal
docker network instead.

The public status page (`/status`, `/status/*`, `/api/public/*`,
`/static/*`, `/robots.txt`, `/favicon.ico`) is **also unauthenticated by
design** — see [Public status surface](#public-status-surface) below.

See [Authentication](authentication.md) for the in-binary flow.

### Email provider (Resend)

Every mail the product sends (sign-in links, invitations, outage alerts, status-page subscriber updates, billing notices) goes through the `EmailSender` trait. Production uses [Resend](https://resend.com); dev and test default to the `log` provider, which writes the action URL to the tracing log so you can copy-paste it into a browser.

Setup:

1. Create a Resend account and verify your sending domain. Resend will
   give you DKIM and DMARC records to add to DNS.
2. Generate an API key with `emails.send` permission only.
3. Configure the service:

   ```toml
   [email]
   provider = "resend"
   from_name = "Acme Status"
   from_address = "status@acme.test"

   [email.resend]
   api_key = "re_…"
   ```

   Or via env: `UPTIMEPAGE_EMAIL__PROVIDER=resend`, `UPTIMEPAGE_EMAIL__FROM_ADDRESS=status@acme.test`, `UPTIMEPAGE_EMAIL__RESEND__API_KEY=re_…`.

   Send from an address somebody reads. Every reply comes back to it, including auto-responders to outage alerts and subscriber updates. A `no-reply@` sender leaves a confused recipient with no way to answer except the spam button, and mailbox providers count replies in the sender's favour.
4. `auth.public_base_url` must be set to the externally-reachable origin
   (e.g. `https://status.acme.test`); the value is embedded in the links
   the recipient receives.

The factory rejects boot when `provider = "resend"` is set without a
non-empty API key — fail-fast over send-time surprise.

### Public status surface

The Caddyfile carries an `@public` matcher that handles the public status paths ahead of the catch-all and adds a per-IP rate limit (60 req/min) via the [`caddy-ratelimit`](https://github.com/mholt/caddy-ratelimit) plugin. The stock `caddy:2-alpine` image doesn't include that plugin, so the production deployment uses a custom `custom-caddy:2` image built with `xcaddy`:

```bash
docker build -t custom-caddy:2 - <<'EOF'
FROM caddy:2-builder AS builder
RUN xcaddy build --with github.com/mholt/caddy-ratelimit

FROM caddy:2-alpine
COPY --from=builder /usr/bin/caddy /usr/bin/caddy
EOF
```

Then point the `caddy` service in `deployment/docker-compose.yml` at `custom-caddy:2`. Full procedure (including the opt-out path that drops the rate-limit block) is in [`deployment/README.md`](https://github.com/uptimepage/uptimepage/tree/main/deployment).

The same custom image carries the other per-IP zones, among them `auth_endpoints` (10/min on `/auth/*`, `/api/v1/me`, invitation accept), `oauth_register` (30/min on `POST /oauth/register`, the open MCP client registration) and `org_creation` (3 per 24 h on `POST /api/v1/orgs`); the full list is in [`deployment/README.md`](https://github.com/uptimepage/uptimepage/tree/main/deployment). These are the edge tier; the per-org / per-user budgets the service enforces from each org's plan are the [Quotas & rate limits](quotas.md) tier — complementary, since behind the proxy the app sees only the proxy as the peer.

#### Per-org subdomains (SaaS)

When `tenancy.subdomain_public_routes = true`, each org's page is served at `{slug}.{public_status.base_domain}` (apex-wildcard shape). That needs:

- a wildcard DNS record `*.{domain}` pointing at the host (plus explicit A/AAAA records for any operator subdomain — `app`, `mail`, etc. — which take precedence over the wildcard);
- a **wildcard TLS cert** for `*.{domain}`. HTTP-01 can't validate a wildcard, so the custom Caddy image also bundles [`caddy-dns/hetzner`](https://github.com/caddy-dns/hetzner) and solves the ACME **DNS-01** challenge using a `HETZNER_DNS_API_TOKEN` (zone-edit scope) from `.env`. The operator host (`app.{domain}`) and the MCP host (`mcp.{domain}`) are kept on their own per-host HTTP-01 certs in separate Caddyfile blocks so a wildcard-key compromise reaches neither the operator surface nor connector bearer tokens. The app serves only `/mcp` and `/.well-known/*` on the MCP host; every other route answers 404 there, the same default-deny the tenant hosts get.

The wildcard means a new org's page works the moment its owner enables it — no per-org DNS or cert step. The end-to-end runbook (Hetzner zone setup, token scope, building the image, verifying the wildcard cert) is in [`deployment/README.md`](https://github.com/uptimepage/uptimepage/tree/main/deployment). The model — host routing, branding, opt-in gating, cookie scoping — is in [Per-org status pages](per-org-status.md).

For the operator workflow (enabling components, narrating incidents, scheduling maintenance) see [Public status page](public-status.md).

## Docker

`docker compose up -d` brings up Postgres 18, ClickHouse 26.3, and the monitor on the same network. It pulls the published image `ghcr.io/uptimepage/uptimepage:latest`; override the tag with `UPTIMEPAGE_IMAGE`, or build from a checkout with `docker compose -f docker-compose.yml -f compose.build.yml up -d --build`. Compose env vars wire the monitor to the stack:

```yaml
UPTIMEPAGE_STORAGE__POSTGRES__URL: postgres://monitor:monitor@postgres:5432/monitor
UPTIMEPAGE_STORAGE__CLICKHOUSE__URL: http://clickhouse:8123
UPTIMEPAGE_STORAGE__CLICKHOUSE__USER: monitor
UPTIMEPAGE_STORAGE__CLICKHOUSE__PASSWORD: monitor
UPTIMEPAGE_STORAGE__ALLOW_DEFAULT_CREDENTIALS: "true"
UPTIMEPAGE_OBSERVABILITY__LOG_FORMAT: json
```

The runtime image is `gcr.io/distroless/static-debian12:nonroot` for a minimal attack surface, no shell, and no glibc. Built from a static musl binary via `rust:1-alpine`. Final image is **16 MB** — both `uptimepage` and `loadtest` binaries fit in the same image.

## Bind addresses

Defaults are loopback (`127.0.0.1:8080` API, `127.0.0.1:9090` metrics). Override via env for non-loopback exposure:

```bash
UPTIMEPAGE_SERVER__API_BIND=0.0.0.0:8080 \
UPTIMEPAGE_SERVER__METRICS_BIND=0.0.0.0:9090 \
./uptimepage
```

The app authenticates its own routes, so this port is not open season, but it speaks plain HTTP and applies no per-IP limits. Terminate TLS in front of it or keep it on a private network. The ready-made Caddy stack under [`deployment/`](#production-deployment-with-caddy) does both.

## Metrics shipping (Grafana Cloud)

The Prometheus `/metrics` endpoint can be shipped to Grafana Cloud by a
Grafana Alloy sidecar. It is **opt-in**: the compose stack only starts it
under the `metrics` profile (`docker compose --profile metrics up -d`),
so the default deployment is unchanged. Credentials are read from `.env`
(gitignored) and never written into `deployment/config.alloy`.

`deployment/README.md` ("Metrics") is the authoritative setup, including
how to obtain the Grafana Cloud URL/token, the internal-network bind, the
ready-made dashboard, and how to verify ingestion.

## Migrations

- Postgres: `migrations/postgres/*.sql`, applied at startup via `sqlx::migrate!` (tracked in `_sqlx_migrations`)
- ClickHouse: `migrations/clickhouse/*.sql`, applied idempotently via `CREATE … IF NOT EXISTS` at startup

No external migrator. The app owns its schema lifecycle symmetrically.

## Resource sizing

- `checker.max_concurrent_checks` caps simultaneous in-flight checks
- Per-check memory: small (a tokio task + an in-flight hyper request + bookkeeping)
- The practical ceiling is set by file descriptors and ephemeral ports, not RAM
- At 50k concurrent checks against external targets, RSS sits around 200-400 MB depending on response sizes
- The optional `metrics` profile adds a Grafana Alloy container (~100 MB RSS plus a small bounded remote-write WAL volume) — account for it when sizing the host if you enable it

## Graceful shutdown

The binary listens for SIGINT and SIGTERM, cancels the scheduler and batcher via a shared `CancellationToken`, awaits both background tasks, and exits within 10 s. The batcher's cancel branch drains any pending results before returning. A warning is logged if the deadline is exceeded.
