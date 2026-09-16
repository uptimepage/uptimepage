# Architecture

uptimepage is one Rust binary that runs multi-tenant uptime monitoring and public status pages. Postgres holds configuration and control-plane state, ClickHouse holds check results, and extra regions are optional stateless probe processes. This page is the map: what the pieces are, how a request and a check flow through them, and which invariants every feature must respect. Deep dives live in the linked pages.

For an interactive companion to this page, open the [flow map](/architecture): pick any runtime path and watch it light up across every process, surface, service, and store.

## Goals

- Run periodic checks of eight kinds (HTTP, TCP, ping, heartbeat, DNS, TLS certificate, domain expiry, browser flow) against an arbitrary, mutable set of targets. See [Monitor types](monitor-types.md).
- Keep every tenant's data isolated by construction, not by convention. See [Multi-tenancy](multi-tenancy.md).
- Turn a run of failing checks into a confirmed incident, page the right people, and publish a customer-facing status surface. See [Incident management](incidents.md) and [Public status page](public-status.md).
- Attribute every result to the region that produced it, so a deployment can probe from many locations with no coordination. See [Multi-region probes](multi-region.md).
- Survive transient target failures (per-host circuit breakers), tenant bursts (per-tenant host throttle), and storage flaps (in-process retry and batching), and count every dropped unit of work with a reason.
- Drain in-flight work on shutdown rather than lose it.

## Process modes

The same binary runs in one of three modes; the mode is chosen at startup, before any subsystem is built.

- **Control plane** (default). Owns Postgres and ClickHouse, the REST API, the web UI, the MCP server, the marketing host, the scheduler for its own region, the incident writer, the escalation engine, public status pages, and the periodic jobs. Its own region is a normal `regions` row, not a sentinel.
- **Agent** (`[agent] enabled = true`). A stateless probe: scheduler, registry, worker pool, batcher, sampler, and dispatch long-poller, and nothing else. No database, no web or API, no alerting. It pulls its region's decrypted monitor config from the control plane over an authenticated Bearer token and POSTs results back; region and agent identity are derived from the token server-side, never from the payload. See [Multi-region probes](multi-region.md).
- **Bootstrap CLI** (`uptimepage bootstrap-owner --email <addr>`). Seeds the first user, org, and owner membership, mints a full-access API token, and exits. Runs against Postgres only. Installs with no terminal use `[bootstrap] email` instead, which seeds the same owner during control-plane startup and logs a sign-in link rather than a token. See [First-run owner](configuration.md#first-run-owner).

## Module layout

```
src/
├── main.rs           startup: config load, validation, mode branch, subsystem spawn
├── lib.rs            crate root (31 modules)
├── app.rs            AppState, the composition root (every store + engine as a field)
├── router.rs         router assembly + middleware layer order
├── config.rs         typed AppConfig + UPTIMEPAGE_ env override loader
├── bootstrap.rs      bootstrap-owner CLI mode + first-run owner seeding
│
├── domain/           core types: Target, CheckSpec, CheckResult, Incident, on_call,
│                     notification_channel, quota; no I/O
├── storage/          Postgres + ClickHouse + in-memory stores behind traits;
│                     admin.rs / operator.rs are the audited tenancy escape hatches;
│                     locks.rs holds the advisory-lock helpers
├── security/         AES-GCM envelope crypto for sealed secrets
│
├── scheduler/        target registry (full re-list + diff) + single-driver timing heap
├── worker/           worker pool + per-host circuit breaker + host throttle +
│                     one executor per check kind (http/tcp/ping/heartbeat/dns/
│                     tls_cert/domain_expiry/flow)
├── http_client/      poolless probe client: phase-timing connector, DNS cache, SSRF guard
├── net/              happy-eyeballs dial (RFC 8305)
├── pipeline/         result batcher (size + timeout flush, bounded, counted drops)
├── ad_hoc_dispatch.rs  check-now / test long-poll to the region's agent
│
├── public_status/    incident writer (poller), status-page aggregator, subscriber dispatch
├── escalation/       paging engine + on-call resolution (feature-flagged, off by default)
├── notifier/         one transport per channel kind + IncidentNotice event
├── email/ telegram/ whatsapp/    transport-specific helpers
├── http_outbound.rs  shared outbound client for webhooks and provider APIs
│
├── api/              REST /api/v1 handlers, routes, OpenAPI doc, stable error envelope
├── web/              server-rendered operator UI (renders only; mutations call the API)
├── mcp/              in-process MCP server (typed, authorized, audited tools)
├── oauth/            OAuth 2.1 authorization server backing the MCP connector
├── auth/             session + API-token + agent-token extractors and scopes
├── marketing/        apex/www/blog/docs/landing pages; no storage or tenancy imports
│
├── quotas/           effective-plan resolution + governor rate limiter
├── jobs/             periodic jobs: retention, two-store erasure, token/session cleanups
├── observability/    tracing + Prometheus + OTLP, gauge sampler, dead-man snitch
├── agent/            stateless agent-mode entry point
├── bin/              the loadtest binary
└── error.rs          AppError -> ApiError envelope

templates/            askama HTML compiled into the binary
static/               rust-embed bundle: Tailwind 4 output (build.rs) + esbuild JS
migrations/           postgres/NNN_name.{up,down}.sql + clickhouse/*.sql
```

## Request path

One `RouteByHost` service (in `src/marketing/dispatch.rs`) inspects the Host header through the single host parser in `src/web/host.rs` and routes by class before any handler runs:

- **Marketing** (apex and `www`): the marketing router, which touches no database.
- **App** (the operator labels, `app` and `mcp`): the full application router. The MCP host is then narrowed to `/mcp` and `/.well-known/*` by the isolation middleware below.
- **Tenant public** (any other subdomain): the application router behind a default-deny fence that allows only the public status, subscribe, public API, and static paths. Everything else 404s, so login and operator routes can never appear on a tenant host.
- **Unknown**: the marketing router with a branded 404.

Inside the application router the middleware order is load-bearing and documented at the top of `src/router.rs`: `http_metrics` outermost, then `host_isolation`, then CSRF. Metrics must observe requests the later guards reject, and a tenant or MCP host must 404 an operator route before CSRF's constant-time compare runs. The `/api/v1` stack adds a body limit, API-token auth, and per-org-then-per-user rate limiting. Reordering these changes request semantics, not just style.

Authentication resolves a session cookie or a scoped `sm_live_` Bearer token into an authorization extractor (`Authorized<Scope>`, `OwnerAuthorized<Scope>`, `CurrentOrg`, and so on). See [Authentication](authentication.md).

## Probe path

By default the control plane schedules and runs every check itself. The pipeline from config to stored result:

```
Postgres (targets)
   │ TargetRegistry.refresh()  full re-list, diffed into added/updated/removed
   ▼
Scheduler        single-driver min-heap keyed by next-due instant; jittered tick;
   │             paused/deleted targets tombstoned in the heap and reaped lazily
   │ dispatch
   ▼
WorkerPool       heartbeat fast path, then in-flight guard, semaphore, circuit
   │             breaker, per-tenant host throttle; each gate that drops work counts it
   │             ├── http / tcp / ping / dns / tls_cert / domain_expiry / flow executors
   │             └── heartbeat (passive dead-man, evaluated inline)
   │ CheckResult on a bounded mpsc channel
   ▼
ResultBatcher    flush on size or timeout; buffer bounded, oldest dropped on overflow,
   │             retries exhausted counted
   ▼
ClickHouse       check_results (per-row TTL) + 1-minute and 1-hour rollup views
```

HTTP checks connect fresh every interval. There is no connection pool, because a monitor probes each target once per interval so a pool would rarely reuse a socket, and connecting fresh is exactly what lets the probe time DNS resolution, TCP connect, and the TLS handshake as separate phases and write them into each result. The connector resolves, applies the SSRF filter, dials with happy eyeballs, and optionally completes TLS, returning per-phase timings. See [Monitor types](monitor-types.md).

On-demand checks (`POST /targets/{id}/check-now` and `POST /targets/test`) are dispatched to the target region's agent over a held long-poll, and the request waits for the result. If no agent is serving the region the request returns `503 PROBE_UNAVAILABLE`. This path is single-process: the claim and the originating request must land on the same control-plane process.

## Detection, incidents, and paging

Results do not page directly. A separate follower turns them into confirmed incidents:

- **Incident writer** (`src/public_status/incident_writer/`) is a poller, not an event listener. On a default 30-second tick it keyset-paginates enabled targets across tenants, reads each target's recent results per a lookback tier, and applies the target's region quorum policy (Any, Majority, All, or Count) to decide up or down. Insert-open and close are race-safe, so exactly one writer pages. This confirmation step is why public status derives from confirmed incidents and never from raw samples.
- **Escalation engine** (`src/escalation/engine.rs`) is feature-flagged and off by default. When on it is the single source of down and up notifications: it opens a paging episode, walks the escalation ladder, renotifies, retries with backoff into a dead-letter, and resolves only to the channels paged this episode. On-call is never stored; who is on call is a pure function resolved at page time. The `escalation.enabled` switch gates only the ladder machinery: on, a policy walks levels and renotifies; off, a monitor's directly bound channels are still paged, just without the ladder.
- **No-data detection** (`src/observability/silence.rs`) handles monitors whose covering regions all went dark, notifying bound channels once per episode. Above a fraction of the fleet it is treated as one infra outage and per-customer notices are suppressed.

Internal incident state (Triggered, Acknowledged, Resolved) and the public communication phase are orthogonal tracks and never share a field. See [Incident management](incidents.md) and [Notifications](notifications.md).

## Data model

Two backends, split by access pattern:

- **Postgres** holds configuration and control-plane state: tenancy, monitors, incidents, paging and on-call, public-status config, auth, quotas, and ops tables. Low-cardinality, mutated by API operations. Migrations run via `sqlx::migrate!` at startup. Three high-churn audit tables (`org_audit_log`, `login_attempts`, `quota_events`) are monthly range-partitioned with a boot-time and daily partition maintainer.
- **ClickHouse** holds check results only: append-only, high-cardinality, queried by time range. `check_results` carries a per-row TTL stamped from the org plan; two AggregatingMergeTree views roll it up to one-minute and one-hour grains. Reads for sparklines, buckets, and summaries go through a rollup, never raw. Migrations are idempotent `CREATE ... IF NOT EXISTS` statements run by a hand-rolled runner; editing a shipped one is a silent no-op on existing volumes, so schema changes are validated against a fresh volume. The result sink retries by re-sending an identical block, which the deduplication window collapses, so mid-batch checkpointing is forbidden.

Erasure spans both stores through a two-step outbox (`src/jobs/purge_deleted.rs`): the Postgres transaction enqueues and cascades in one shot, and the ClickHouse side drains idempotently and only settles a queue row after a count proves zero rows across all three result tables.

Every tenant-facing storage method takes `org: OrgId` as its first parameter, every child table carries a denormalised `org_id` with a parent-match trigger, and the isolation is enforced at the type, schema, script, and test level. The only ways around it are the two audited escape hatches (`src/storage/admin.rs`, which requires a static reason string, and `src/storage/operator.rs`, which is global by nature). See [Multi-tenancy](multi-tenancy.md).

## Key design choices

- **Sealed secrets.** Channel and variable secrets are sealed with one AES-GCM envelope at the storage edge and redacted on every read. A read-then-write round trip that resubmits the redaction sentinel is rejected, so a UI edit can never overwrite a secret with its own mask. See [Variables and secrets](variables.md).
- **Quota in the insert.** Handler-level quota checks exist only for a friendly error. The race-safe guarantee is `(count) + 1 <= limit` evaluated inside the INSERT under a per-org advisory lock. See [Quotas and rate limits](quotas.md).
- **Explicit back-pressure.** Every bounded queue and gate that drops work increments `uptimepage_storage_dropped_results_total` with a reason label. A dropped result is always counted, never swallowed.
- **Per-host circuit breakers and a per-tenant host throttle.** A failing host opens its breaker and subsequent checks fail fast without consuming a worker slot. A separate fail-fast semaphore, keyed per tenant, caps how many in-flight checks one tenant can run against the same host and port, so one customer's burst cannot starve another. Throttle drops are recorded as degraded and do not page; the upstream is fine, the back-pressure is operator-side.
- **Sticky last-good for domain expiry.** Each successful RDAP probe writes the expiry, registrar, and last-success time. A subsequent transient failure serves the cached verdict with a `served_stale:` annotation rather than flipping the monitor; only staleness past a threshold escalates to an alert-eligible error. Concurrent probes for the same domain are collapsed to one outbound request.
- **Self-describing API.** `utoipa` derives an OpenAPI 3.1 document at compile time, served at `/api/openapi.json` and rendered at `/docs`. The 4xx/5xx error envelope and the list page envelope are unified across every endpoint. See [REST API](api.md).
- **Cancellation tokens for shutdown.** The root token is cloned into the scheduler, batcher, sampler, jobs, and the graceful axum shutdown. SIGINT or SIGTERM cancels the root and subsystems drain together.

## Concurrency model

- One multi-threaded Tokio runtime.
- One scheduler driver task owns a min-heap of `(due, target)` keyed by next-due instant. It sleeps until the earliest due entry, dispatches every due target, and reschedules at interval plus jitter. Memory stays flat in the fleet size rather than one task and timer per target. Paused and deleted targets are tombstoned by sequence number in a side map and reaped lazily on pop, not removed from the heap.
- `WorkerPool` spawns a task per dispatch, gated by a semaphore sized to the configured concurrency; the heartbeat check runs inline with no spawn, permit, breaker, or throttle.
- The batcher, sampler, and each periodic job are single tasks driven by `tokio::select!` over their trigger and the cancellation token. Every periodic job runs under a Postgres advisory lock keyed by job name, so in a multi-replica deployment only one replica executes it.

## Where region partitioning lives

Region assignment is not in the scheduler. It lives in the `EnabledTargetSource` implementation in `src/storage/admin.rs` (`RegionTargetSource`, `HeartbeatTargetSource`, `AgentPullSource`), and the assignment table is `target_regions`. Results carry their region as a low-cardinality column through both ClickHouse rollups, so reads can slice by region and quorum policies can require agreement across regions. See [Multi-region probes](multi-region.md).

## Related pages

[Multi-tenancy](multi-tenancy.md) · [Multi-region probes](multi-region.md) · [Monitor types](monitor-types.md) · [Incident management](incidents.md) · [Notifications](notifications.md) · [Public status page](public-status.md) · [Authentication](authentication.md) · [Quotas and rate limits](quotas.md) · [MCP server](mcp.md) · [Configuration](configuration.md) · [Development](development.md)
