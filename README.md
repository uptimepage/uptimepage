<div align="center">

<img src="static/marketing/og.png" alt="uptimepage" width="640">

# uptimepage

**Status pages + uptime monitoring. Open source, free to start. Live in 5 minutes.**

Monitor HTTP, TCP, ICMP ping, cron-job heartbeats, DNS, TLS-certificate and
domain expiry, plus scripted browser login flows, from multiple regions — then
turn green and red into a polished
public status page your customers can subscribe to. Drive it by click, REST
API, or Terraform. Self-host the single binary or use the hosted service.

[![Terraform Registry](https://img.shields.io/badge/terraform-registry-7B42BC?logo=terraform&logoColor=white)](https://registry.terraform.io/providers/uptimepage/uptimepage)
[![Artifact Hub](https://img.shields.io/endpoint?url=https://artifacthub.io/badge/repository/uptimepage)](https://artifacthub.io/packages/search?repo=uptimepage)
[![Artifact Hub](https://img.shields.io/endpoint?url=https://artifacthub.io/badge/repository/uptimepage-agent)](https://artifacthub.io/packages/search?repo=uptimepage-agent)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)

[**Try it free →**](https://uptimepage.dev)&nbsp;&nbsp;·&nbsp;&nbsp;[Docs](https://uptimepage.dev/docs)&nbsp;&nbsp;·&nbsp;&nbsp;[Self-host](#self-host)&nbsp;&nbsp;·&nbsp;&nbsp;[Terraform](#terraform)&nbsp;&nbsp;·&nbsp;&nbsp;[MCP](#mcp-server)

<img src="static/marketing/uptime-monitoring-dashboard.webp" alt="uptimepage dashboard — uptime, response time, and per-monitor status" width="900">

</div>

## Quick start

Hosted, no install:

1. Sign up at **[uptimepage.dev](https://uptimepage.dev)** with GitHub, Google or your email address — no card.
2. Add a monitor: paste a URL, pick a check type and interval, save.
3. Bind a notification channel (Slack, email, PagerDuty, …) so failures reach you.
4. Turn on a public status page and share the link.

Prefer code? Drive the same account by [REST API](docs/api.md), [Terraform](#terraform), or [MCP](#mcp-server). Want to run it yourself? Go to [Self-host](#self-host).

## Why uptimepage

Monitoring with a built-in status page isn't new — the bet here is doing it
free, self-hostable, and fully as code:

- **Free hosted tier, no card** — and AGPL if you'd rather run it yourself.
- **One self-contained binary** — `docker compose up` and you're live, not a Kubernetes platform to operate.
- **Everything as code** — REST API, scoped tokens, an official Terraform provider, and an MCP server your LLM can query.
- **Probes you own** — run multi-region agents wherever your users are, on your own boxes.
- **Status page + incidents + alerting in one** — components, subscribers, and multi-channel paging that repeats until acknowledged, no second tool.
- **Core isn't paywalled** — checks, status pages, subscribers, the API and every alert channel are in the free tier.

### Who it's for

- **Founders / small SaaS** — a professional status page in minutes, free, no self-host headache.
- **Platform / SRE leads** — monitors as code, multi-region probes, incident paging to your on-call channels, without vendor sprawl.
- **Self-hosters** — one binary plus two databases, no SaaS lock-in.

## Live status

uptimepage monitors itself. These badges are served by the running app from a
public status page — no third-party service, no cron job updating a JSON file.

[![uptimepage status](https://uptimepage.uptimepage.dev/api/public/v1/badge.svg)](https://uptimepage.uptimepage.dev)

| Surface | HTTP | DNS | TLS cert | Domain |
|---|---|---|---|---|
| **uptimepage.dev** (website) | [![http](https://uptimepage.uptimepage.dev/api/public/v1/badge.svg?component=9ffa993c-fc72-4a54-a7d7-259249cd30eb)](https://uptimepage.uptimepage.dev) | [![dns](https://uptimepage.uptimepage.dev/api/public/v1/badge.svg?component=480a9ed0-dc21-4042-9cc3-f67814bc75be)](https://uptimepage.uptimepage.dev) | [![tls](https://uptimepage.uptimepage.dev/api/public/v1/badge.svg?component=70083a5c-0463-4c1e-99e5-c780473a96cf)](https://uptimepage.uptimepage.dev) | [![domain](https://uptimepage.uptimepage.dev/api/public/v1/badge.svg?component=cc34930e-a1d8-41a3-a448-722fec4c28f4)](https://uptimepage.uptimepage.dev) |
| **app.uptimepage.dev** (dashboard) | [![http](https://uptimepage.uptimepage.dev/api/public/v1/badge.svg?component=c2510a68-d830-4e93-b8db-28d0ec5e05ff)](https://uptimepage.uptimepage.dev) | | | |
| **mcp.uptimepage.dev** (MCP) | [![http](https://uptimepage.uptimepage.dev/api/public/v1/badge.svg?component=49f2209c-d54e-49fd-a38a-bc5f03b9f77f)](https://uptimepage.uptimepage.dev) | | | |

Embed your own with the snippet in **Settings → Pages → your page → Badge**.

## Features

| | |
|---|---|
| **Checks** | HTTP, TCP, ICMP ping, heartbeat (inbound dead-man's-switch), DNS, TLS-cert expiry, domain expiry, browser login flow with per-step timings — per-host circuit breaking, designed for ~50k concurrent in-flight |
| **Public status page** | HTML + JSON + RSS, per-component opt-in, incident narration, maintenance windows, email + webhook subscribers |
| **Alerting** | Slack, PagerDuty, Discord, Microsoft Teams, Google Chat, Mattermost, Telegram, WhatsApp, SMS, email, webhook, ntfy, Gotify, Pushover — per-org channels, sealed secrets, fire-once + recovery, repeat until acknowledged |
| **Incidents** | Internal incident state ⊥ public phase, acknowledge to silence paging, per-monitor reminder cadence |
| **Multi-region** | Regional probe agents, per-region views, run your own agent anywhere |
| **Automation** | REST API, scoped API tokens, Terraform provider, MCP server for LLM clients |
| **Built on** | Rust 1.95 / Tokio / Axum, Postgres + ClickHouse, one ~23 MB self-contained binary |

**Live service: <https://uptimepage.dev>** — hosted, free, sign in with GitHub, Google or your email address.
**Full docs: <https://uptimepage.dev/docs>**

<div align="center">

<img src="static/marketing/uptime-monitors-list.webp" alt="The monitors list: checks grouped by environment, each with live status, type, tags, last check time, and 30-day uptime." width="900">
<br><sub>Monitors — grouped by environment, filterable by type, tag, or owner</sub>

</div>

## Check types

| Type | Purpose | Default interval | Floor |
|---|---|---|---|
| `http` | request a URL, match status / body / latency | 60 s | `max(plan_min, 10 s)` |
| `tcp` | open a TCP socket within a timeout | 60 s | `max(plan_min, 10 s)` |
| `ping` | ICMP echo request, fail on a missing reply | 60 s | `max(plan_min, 10 s)` |
| `heartbeat` | your job pings a URL; a missing ping past period + grace opens an incident | 60 s | `max(plan_min, 60 s)` |
| `dns` | resolve a record, optionally match a value | 60 s | `max(plan_min, 10 s)` |
| `tls_cert` | open TLS, parse leaf cert, alert before `notAfter` | 86 400 s (daily) | `max(plan_min, 3600 s)` |
| `domain_expiry` | query RDAP, alert before the domain's `expiration` event | 86 400 s (daily) | `max(plan_min, 43 200 s)` |
| `flow` | drive a headless browser through login / transaction steps, assert the result | 300 s | `max(plan_min, 300 s)` |

`tls_cert` and `domain_expiry` use `warn_days` / `critical_days` thresholds and surface `days_remaining` plus registrar / cert subject in the result payload. Their floors are 1 hour for `tls_cert` and 12 hours for `domain_expiry`, regardless of plan — these probes track values that change on a scale of days, not minutes, and RDAP rate-limits by source address. `flow` runs a real browser, so it only executes where a browser engine is available (its regions clamp to the flow-capable set) and the number of flow monitors is capped per plan; put credentials in an org secret and reference them as `{{name}}`. Every flow run is kept with each step's outcome and duration, and the monitor page charts each step on its own scale, so a wait drifting from 200 ms to four seconds shows up long before the journey fails. A self-hosted install starts with that cap at 0; [docs/monitor-types.md](docs/monitor-types.md#flow) covers turning it on. See [docs/api.md](docs/api.md) for the full payload shapes.

## Public status page

A customer-facing `/status` page (HTML + JSON + RSS 2.0) is built into the binary. Per-target opt-in via `public_status`; the page's routes carry no auth extractor, so it is public wherever the binary runs, with a per-IP rate limit at the edge, caches for 10 s in-process, and degrades gracefully if ClickHouse is unreachable. Operators narrate incidents (`PATCH /api/v1/incidents/{id}`, `POST /api/v1/incidents/{id}/updates`) and schedule maintenance windows (`POST /api/v1/maintenance`). Visitors subscribe for email or webhook updates. See [docs/public-status.md](docs/public-status.md).

<div align="center">

<img src="static/marketing/public-status-page.webp" alt="A public status page reading All Systems Operational, with 90-day uptime history per component." width="900">
<br><sub>Public status page — 90 days of history per component</sub>

</div>

## Alerting

Notification channels are per-org resources (Slack incoming webhook, generic
HTTP webhook, Telegram bot, SMS gateway, …) created via `/api/v1/notification-channels`.
Transport secrets are sealed at rest and never echoed back.

<div align="center">

<img src="static/marketing/notification-channels-slack-pagerduty-sms.webp" alt="The notification channels list: Slack, PagerDuty, SMS, email, webhook, Discord, Teams, Pushover, ntfy, and Telegram, each enabled or disabled." width="900">
<br><sub>Alerting — Slack, PagerDuty, SMS, webhook, and seven more</sub>

</div>

A target opts in by binding one or more channels in its `alerts` array:

```jsonc
"alerts": [
  { "channel_id": "0192…", "after_failures": 3 },
  { "channel_id": "0193…", "after_failures": 6, "notify_recovery": false }
]
```

Fire-once + recovery semantics. Channels are tenant-isolated — a target can
only bind a channel its own org owns. See [docs/api.md](docs/api.md) for the
full contract.

<div align="center">

<img src="static/marketing/add-notification-channel.webp" alt="The new notification-channel form: choose a channel type such as Slack, Discord, email, Telegram, PagerDuty, SMS, or webhook." width="900">
<br><sub>Adding a channel — pick a transport, paste one credential</sub>

</div>

## Multi-region

Run probe agents in the regions your users live in; the control plane assigns
checks per region and the dashboard and status page can be viewed per-region.
Bring your own agent anywhere — a single binary with an org-scoped token. See
[docs/multi-region.md](docs/multi-region.md).

<div align="center">

<img src="static/marketing/monitor-latency-by-region.webp" alt="One monitor in detail: uptime and check counts for the last 24 hours, plus median latency charted per region." width="900">
<br><sub>Monitor detail — latency broken down by region</sub>

</div>

## Terraform

Manage targets and notification channels as code with the official provider
[`uptimepage/uptimepage`](https://registry.terraform.io/providers/uptimepage/uptimepage)
([source](https://github.com/uptimepage/terraform-provider-uptimepage)):

```hcl
terraform {
  required_providers {
    uptimepage = {
      source = "uptimepage/uptimepage"
    }
  }
}

provider "uptimepage" {
  token = var.uptimepage_token # or set UPTIMEPAGE_TOKEN
  org   = "your-org-slug"      # required for managed resources; or UPTIMEPAGE_ORG
  # endpoint defaults to https://app.uptimepage.dev; set it for a self-hosted instance
}

resource "uptimepage_target" "api" {
  name     = "api prod"
  interval = 60
  check = {
    type = "http"
    http = {
      url             = "https://example.com/healthz"
      expected_status = { kind = "exact", exact = 200 }
    }
  }
}
```

## MCP server

An [MCP](https://modelcontextprotocol.io/docs/getting-started/intro) server lets an LLM client (the claude.ai connector, Claude Desktop, an IDE) answer questions about one org's monitors and take a few guarded actions, over Streamable HTTP at `/mcp`. Sixteen read-only tools plus fifteen write tools (each scope-gated, confirmed per action, and audited; a client that can't show a confirmation is offered the read tools only). Auth is an org-bound scoped token — paste one by hand, or use the one-click OAuth 2.1 connector. Off by default; enable with `UPTIMEPAGE_MCP_ENABLED=true` (`+ MCP_OAUTH_ENABLED` for the connector). See [docs/mcp.md](docs/mcp.md).

## Self-host

| I want to | Go to |
|---|---|
| Try it with one command | [Docker](#docker-recommended) |
| Run it for real, with TLS and auth | [Production deployment](#production-deployment) |
| Build the image myself (ARM host, or code changes) | [Build from source](#build-from-source) |
| Skip Docker and run the binary | [Run without Docker](#run-without-docker) |

### Docker (recommended)

Three steps. No Rust toolchain and no compile step.

**1. Start it**

```bash
docker compose up -d
```

Pulls `ghcr.io/uptimepage/uptimepage:latest` and starts Postgres 18, ClickHouse 26.3, and the monitor. Both databases run their own migrations at startup, so there is nothing to wire up.

**2. Create your account**

```bash
docker compose exec uptimepage uptimepage bootstrap-owner --email you@example.com
```

Prints a one-time sign-in link, a full-access API token, and your org slug. These appear once, so copy them before closing the terminal.

**3. Sign in and add a monitor**

Open the link from step 2. That drops you into the dashboard at `http://localhost:8080`, where you can add your first monitor.

To stop: `docker compose down`, or `docker compose down -v` to delete the data too.

> This stack has no TLS and no auth in front of it, and it publishes the database ports. Great for trying it out, not safe on the open internet. To run it for real, see [Production deployment](#production-deployment).

#### Options

**Pin a version.** Set `UPTIMEPAGE_IMAGE=ghcr.io/uptimepage/uptimepage:1.3.0` to stay on a release instead of `latest`. Release tags carry no `v` prefix, though the git tag they are built from does.

**On an ARM host?** Release tags are `linux/amd64` and `linux/arm64`. `latest`, which tracks the newest commit on `main`, is amd64 only, so pin a release on ARM.

**Signing in later.** The bootstrap link works once. For repeat sign-in, and to invite a team, set up OAuth with GitHub, GitLab, Google or Microsoft, or an email provider for magic links (`[auth.github]`, `[auth.gitlab]`, `[auth.google]`, `[auth.microsoft]`, `[email]` in `config/default.toml`, or the matching env vars). See [docs/authentication.md](docs/authentication.md).

**Packaging this for an app store?** Step 2 needs a terminal. Set `UPTIMEPAGE_BOOTSTRAP__EMAIL` instead and the first boot seeds that owner and logs a sign-in link, no shell required. See [First-run owner](docs/configuration.md#first-run-owner).

<details>
<summary><b>Prefer the API to the dashboard?</b></summary>

Use the token and org slug from step 2. Create a monitor:

```bash
curl -X POST http://127.0.0.1:8080/api/v1/targets \
  -H 'authorization: Bearer <token>' \
  -H 'x-uptimepage-org: <org-slug>' \
  -H 'content-type: application/json' \
  -d '{
    "name": "example",
    "check": {
      "type": "http",
      "url": "https://example.com/",
      "method": "GET",
      "timeout": 10000,
      "follow_redirects": false,
      "max_redirects": 0,
      "expected_status": { "kind": "exact", "value": 200 },
      "headers": {},
      "verify_tls": true
    },
    "interval": 180,
    "enabled": true,
    "tags": []
  }'
```

`interval` is in seconds and has to meet the plan minimum, which is 180 on the free plan the `bootstrap-owner` step above starts you on. Anything lower returns `422 MIN_CHECK_INTERVAL`. Seeding the owner at first boot instead (see [First-run owner](docs/configuration.md#first-run-owner)) places that org on `quotas.default_plan`, which defaults to `team` and a 30-second floor. An org created later through signup lands on `founding` while that tier is open, where the floor is 60 seconds.

Read uptime and scrape metrics:

```bash
curl -H 'authorization: Bearer <token>' -H 'x-uptimepage-org: <org-slug>' \
  http://127.0.0.1:8080/api/v1/targets/<id>/uptime
curl http://127.0.0.1:9090/metrics
```

</details>

### Production deployment

Anything reachable from the internet should run this stack, not the one above. It adds a Caddy edge with automatic TLS, internal-only Postgres and ClickHouse, per-IP rate limits, and blue/green restarts. The operator host is guarded by the app's own sign-in, not by an edge credential. It pulls the same published image. Setup runbook is in [`deployment/README.md`](deployment/README.md), with the architecture in [docs/deployment.md](docs/deployment.md).

### Kubernetes

```bash
kubectl create namespace uptimepage
kubectl -n uptimepage create secret generic uptimepage \
  --from-literal=fingerprint-salt="$(openssl rand -base64 32)" \
  --from-literal=credentials-kek-base64="$(openssl rand -base64 32)" \
  --from-literal=postgres-url='postgres://uptimepage:pw@pg.internal:5432/uptimepage?sslmode=require' \
  --from-literal=clickhouse-password='pw'

helm install uptimepage oci://ghcr.io/uptimepage/charts/uptimepage \
  --namespace uptimepage \
  --set domain=status.example.com \
  --set clickhouse.url=https://ch.internal:8443 \
  --set secrets.existingSecret=uptimepage \
  --set postgresql.existingSecret=uptimepage \
  --set clickhouse.existingSecret=uptimepage
```

Bring your own Postgres 18 and ClickHouse; the chart ships neither. A second chart, `uptimepage-agent`, runs a probe on its own in another region or inside a private network, against this or the hosted service. Details in [docs/kubernetes.md](docs/kubernetes.md) and [`charts/`](charts/).

### Build from source

Published images are `linux/amd64`. Build from this checkout when you are on an ARM host (Apple silicon, Ampere), or when you are changing the code:

```bash
docker compose -f docker-compose.yml -f compose.build.yml up -d --build
```

Same stack and the same three steps as [Docker](#docker-recommended), only the image origin differs. Note that your local build then owns that image tag, so run `docker compose pull` when you want the published image back.

### Run without Docker

```bash
cargo build --release
./target/release/uptimepage
```

Requires Postgres and ClickHouse reachable at the URLs in `config/default.toml`, plus a non-empty fingerprint salt (the app refuses to boot without one):

```bash
export UPTIMEPAGE_AUTH__FINGERPRINT_SALT=$(openssl rand -base64 32)
```

To run against the compose stack without rebuilding the container:

```bash
docker compose up -d postgres clickhouse
cargo run --release
```

## Docs

Hosted: <https://uptimepage.dev/docs>

Sources under [`docs/`](docs/) — readable directly on GitHub too:

| File | Covers |
|---|---|
| [docs/architecture.md](docs/architecture.md) | goals, module layout, data flow, key design choices, concurrency model |
| [docs/api.md](docs/api.md) | REST endpoints, check-spec payload shapes, result + uptime queries |
| [docs/public-status.md](docs/public-status.md) | operator guide to the public `/status` page: components, incidents, maintenance |
| [docs/authentication.md](docs/authentication.md) | sign-in, sessions, scoped API tokens, org binding |
| [docs/multi-region.md](docs/multi-region.md) | regional probe agents, the operator surface, running an agent, per-region views |
| [docs/mcp.md](docs/mcp.md) | MCP server for LLM clients: tools, scopes, OAuth connector, enabling, examples |
| [docs/configuration.md](docs/configuration.md) | `default.toml` reference, env override scheme, tuning notes |
| [docs/metrics.md](docs/metrics.md) | Prometheus series (incl. connect / TLS / pool gauges), OpenTelemetry tracing |
| [docs/deployment.md](docs/deployment.md) | Docker, bind addresses, migrations, sizing, graceful shutdown |
| [docs/development.md](docs/development.md) | local dev workflow, the web UI (stack, routes, adding a page, tests), faster builds |
| [docs/loadtest.md](docs/loadtest.md) | `bin/loadtest` envs, macOS gotchas, HTTP/1 vs h2c trade-off, Linux container path |
| [docs/benchmarks.md](docs/benchmarks.md) | Criterion micro-benchmarks, single-core throughput, profile breakdown |
| [docs/troubleshooting.md](docs/troubleshooting.md) | common failures and how to read them off metrics |

## Web UI

The single binary serves both the `/api/v1/*` JSON surface and a server-rendered HTML UI at `/` — askama compile-time templates, HTMX for partial swaps and JSON forms (no SPA framework), Tailwind CSS 4, and lazy-loaded ECharts. Every UI mutation hits an existing `/api/v1/*` endpoint, so the API stays the single source of truth. Stack, routes, the add-a-page recipe, and UI tests are in [docs/development.md](docs/development.md#web-ui).

## Legal

A running instance serves its policies at `/terms`, `/privacy`, `/cookies`,
`/impressum`, `/abuse-policy`, `/security-policy`, and an RFC 9116
`/.well-known/security.txt`. The source documents are in
[`docs/legal/`](docs/legal/). GDPR self-service (data export, account
deletion, recovery) lives under `/settings/account`.

## License

uptimepage is licensed under [AGPL-3.0](LICENSE). See [LICENSING.md](LICENSING.md)
for what this means in practice.

If you'd like to contribute, see [CONTRIBUTING.md](CONTRIBUTING.md).
For security disclosures, see [SECURITY.md](SECURITY.md).
</content>
</invoke>
