# REST API

Mounted under `/api/v1` on the configured API bind. JSON in, JSON out. Every `/api/v1/*` endpoint is authenticated: a session cookie from the operator UI, or an `Authorization: Bearer` API token bound to one org and carrying its own scopes. The health probes, the OpenAPI document, and `POST /api/v1/invitations/decline` (token in body, not a session or API token) are the exceptions and stay open. See [Authentication](authentication.md#api-token-auth).

OpenAPI 3.1 document at `GET /api/openapi.json`; Swagger UI at `GET /docs` on the app host (<https://app.uptimepage.dev/docs> on the hosted service).

All responses use `Content-Type: application/json; charset=utf-8`.

### Discovery

Automated clients that only know the site can find the API without being told its paths. The marketing host publishes an [RFC 9727](https://www.rfc-editor.org/rfc/rfc9727) catalog at `GET /.well-known/api-catalog` (`application/linkset+json`) listing the REST API and, when enabled, the MCP server, each with links to its OpenAPI document and its documentation page. Every marketing page also carries an [RFC 8288](https://www.rfc-editor.org/rfc/rfc8288) `Link` header with `rel="api-catalog"`, `rel="service-desc"` and `rel="service-doc"`, so one `HEAD /` is enough to bootstrap. The catalog reflects the running deployment, so a self-hosted install advertises its own URLs.

When the MCP server is enabled it publishes a Server Card at `GET /.well-known/mcp/server-card.json` on the MCP host, built from the same server info the protocol's `initialize` returns. The marketing host redirects the same path there, so an agent that only knows the domain still finds it.

Documentation pages, blog posts and the homepage also answer `Accept: text/markdown` with their Markdown source instead of HTML, at `text/markdown; charset=utf-8` with a `x-markdown-tokens` size hint. Responses carry `Vary: Accept` and a per-representation `ETag`, so browsers keep the rendered page.

### Response headers

- `POST /api/v1/targets` (201) sets `Location: /api/v1/targets/{id}` so clients can follow up without re-deriving the path.
- `Cache-Control` is stamped on every `/api/v1/*` response:
  - mutations (POST / PATCH / DELETE) → `no-store`
  - `/api/v1/dashboard/summary` → `private, max-age=5` (matches the server-side cache)
  - all other reads → `private, max-age=10`

## Endpoints

| Method | Path | Purpose |
|--------|------|---------|
| `POST` | `/api/v1/targets` | create one target |
| `POST` | `/api/v1/targets/bulk` | bulk-create up to 10,000 targets |
| `POST` | `/api/v1/targets/bulk-action` | enable / disable / delete / tag-add / tag-remove / set-group on many ids |
| `POST` | `/api/v1/targets/test` | run a one-shot check against a `CheckSpec` without persisting |
| `POST` | `/api/v1/targets/{id}/check-now` | run an immediate check using the target's stored credentials |
| `GET` | `/api/v1/targets` | list targets (`limit`, `offset`, `tag`, `enabled`, `q`) — paginated |
| `GET` | `/api/v1/targets/{id}` | get one target |
| `PATCH` | `/api/v1/targets/{id}` | update name, check spec, interval, enabled, tags |
| `DELETE` | `/api/v1/targets/{id}` | delete a target |
| `GET` | `/api/v1/targets/{id}/results` | recent check results (`from`, `to`, `limit`, `offset`, `region`) — paginated |
| `GET` | `/api/v1/targets/{id}/latency` | bucketed latency series (`from`, `to`, `region`) — server-side quantiles + per-phase means |
| `GET` | `/api/v1/targets/{id}/latency/by-region` | per-region latency series (`from`, `to`) — one series per region, for overlay charts |
| `GET` | `/api/v1/targets/{id}/flow-steps` | per-step duration series for a browser flow (`from`, `to`, `region`) — one series per journey step |
| `GET` | `/api/v1/targets/{id}/uptime` | uptime summary over a range (`from`, `to`, `region`) |
| `GET` | `/api/v1/targets/{id}/regions` | list the regions a monitor probes from |
| `PUT` | `/api/v1/targets/{id}/regions` | set the regions a monitor probes from |
| `GET` | `/api/v1/targets/{id}/heartbeat` | a heartbeat monitor's ping URL and last reported run |
| `POST` | `/api/v1/targets/{id}/heartbeat/rotate` | mint a replacement ping URL; the old one keeps working for 24 h unless `revoke_previous_immediately` |
| `DELETE` | `/api/v1/targets/{id}/heartbeat/previous` | end a rotation's overlap window early |
| `GET` | `/api/v1/regions` | list the enabled probe-region catalog: `{ "regions": [...] }`, each entry `id`, `name`, `city`, `country_code`, `continent`, `latitude`, `longitude` |
| `GET` | `/api/v1/targets/{id}/incidents` | coalesced incident periods (`from`, `to`, `ongoing_only`) — paginated |
| `POST` | `/api/v1/targets/{id}/shares` | mint a read-only share link; returns the share (token included) |
| `GET` | `/api/v1/targets/{id}/shares` | list a monitor's live share links (token included, re-copyable) |
| `DELETE` | `/api/v1/targets/{id}/shares/{share_id}` | revoke a share link |
| `GET` | `/api/v1/tags` | tag inventory with target counts (`q` prefix) — paginated |
| `GET` | `/api/v1/dashboard/summary` | per-org rollup (5-second in-process cache, keyed by `OrgId`) |
| `GET` | `/healthz` | liveness — always 200 once the process is up |
| `GET` | `/readyz` | readiness — pings the target store; 503 if unreachable |
| `GET` | `/api/openapi.json` | OpenAPI 3.1 document |
| `GET` | `/docs` | Swagger UI |

### Instance-admin and agent surfaces

Two surfaces sit outside `/api/v1` with their own auth:

- `/operator/*` — instance-admin surface, gated by a static bearer secret (`UPTIMEPAGE_OPERATOR__ADMIN_TOKEN`); `404`s when unset. Regions + agents CRUD for multi-region deployments (see [Multi-region](multi-region.md)), and an account's plan and cap overrides (see [Quotas](quotas.md#moving-an-account-between-plans)).
- `/api/agent/*` — the pull/ingest endpoints an agent uses, authenticated by its `sm_agent_…` token (not a tenant `api_token`).

Both are documented in [Multi-region probes](multi-region.md).

### Operator endpoints (maintenance + incident narration)

These mutate the public surface; they live under the same auth boundary as
`/api/v1/targets`. Operator workflow + validation rules in
[Public status page](public-status.md).

| Method | Path | Purpose |
|--------|------|---------|
| `POST` | `/api/v1/maintenance` | schedule a maintenance window |
| `GET` | `/api/v1/maintenance` | list windows (`status=active\|upcoming\|past\|all`, paginated) |
| `GET` | `/api/v1/maintenance/{id}` | get one window |
| `PATCH` | `/api/v1/maintenance/{id}` | edit title / description / time range / components / alert suppression (rejected after `ends_at`) |
| `DELETE` | `/api/v1/maintenance/{id}` | cancel a window |
| `PATCH` | `/api/v1/incidents/{id}` | update narration: `public_title`, `public_description`, `severity` (JSON `null` clears, omit to leave alone), plus `counts_as_downtime` on a declared incident (`422` on a monitor-opened one) |
| `POST` | `/api/v1/incidents/{id}/updates` | append a status update — `phase` ∈ `investigating`/`identified`/`monitoring`/`resolved`/`postmortem`, `message` ≤ 2 000 chars |

### Operator endpoints (status pages)

An org owns one or more public status pages, each with its own slug, branding,
and curated set of monitors. Reads are open to any active member; every mutation
is owner-only. Scoped to the caller's active org (a foreign page id is 404).
Adding a monitor already on the page returns 409 `COMPONENT_ALREADY_ON_PAGE` —
edit it with `PATCH`. Model + caps in [Per-org status pages](per-org-status.md).

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/api/v1/status-pages` | list this org's pages |
| `POST` | `/api/v1/status-pages` | create a page (capped at `max_status_pages`; slug globally unique) |
| `GET` | `/api/v1/status-pages/{id}` | one page + its live URL and logo URL |
| `PATCH` | `/api/v1/status-pages/{id}` | rename, change slug, publish/unpublish, edit branding |
| `DELETE` | `/api/v1/status-pages/{id}` | delete the page |
| `GET` | `/api/v1/status-pages/{id}/components` | the monitors curated onto the page |
| `POST` | `/api/v1/status-pages/{id}/components` | add a monitor (distinct-target cap `max_public_components`) |
| `PATCH` | `/api/v1/status-pages/{id}/components/{target_id}` | per-page `public_name` / `public_description` / `public_group` (JSON `null` clears) |
| `DELETE` | `/api/v1/status-pages/{id}/components/{target_id}` | remove a monitor from the page |
| `POST` | `/api/v1/status-pages/{id}/components/reorder` | set component order |
| `POST` | `/api/v1/status-pages/{id}/logo` | upload a logo (multipart) |
| `DELETE` | `/api/v1/status-pages/{id}/logo` | remove the logo |

### Public status endpoints

Unauthenticated; mounted at `/api/public/v1/*` and bypassed at Caddy via the
`@public` matcher (see [Deployment](deployment.md#public-status-surface)).
Each response carries `Cache-Control: public, max-age=10,
stale-while-revalidate=30`. A monitor not curated onto the page being
served is invisible on every public surface — direct lookups return 404
and it never appears in any list. Wire types literally cannot serialise
sensitive target fields (`url`, `headers`, `basic_auth`, `bearer_token`).

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/` | server-rendered HTML status page on a page's own host (`?fragment=1` returns the dynamic region only) |
| `GET` | `/status` | the same page on a single-tenant deploy, where `/` is the operator dashboard; on a page's own host it redirects to `/` |
| `GET` | `/status/incidents/{id}` | per-incident detail page |
| `GET` | `/api/public/v1/status` | the same data as `/status` in JSON |
| `GET` | `/api/public/v1/components/{id}/history` | per-component history (`days` query, 1..365, default 90) |
| `GET` | `/api/public/v1/incidents` | recent public incidents (paginated) |
| `GET` | `/api/public/v1/incidents/{id}` | one public incident with its update timeline |
| `GET` | `/api/public/v1/incidents.rss` | RSS 2.0 feed of recent incidents |
| `GET` | `/api/public/v1/maintenance` | active + upcoming maintenance windows |
| `GET` | `/api/public/v1/badge.svg` | embeddable SVG status badge (overall, or `?component={id}`) |

See [Public status page](public-status.md) for the operator workflow and
the per-page component fields (`public_name`, `public_description`,
`public_group`, `sort_order`) that drive what's published.

### Operator endpoints (share links)

A share link is a capability URL that renders one monitor's full read-only detail view to anyone who has it, no account. Managing share links — mint, list, revoke — is a monitor action gated on member-level `targets:write` (not owner-only): a read-only member can't call list, so can't harvest a working link that way; a member who can write already has the run of the monitor, so the list response returns the live token too, keeping an existing link re-copyable instead of forcing a new mint. Scoped to the caller's active org (a foreign monitor id is 404). `expires_at` is optional; omit it for a link that never expires. The public surface those tokens unlock is documented in [Share links](share-links.md).

| Method | Path | Purpose |
|--------|------|---------|
| `POST` | `/api/v1/targets/{id}/shares` | mint a share; body `{ "label"?, "expires_at"? }`, returns the `MonitorShare` |
| `GET` | `/api/v1/targets/{id}/shares` | list live (non-revoked) shares |
| `DELETE` | `/api/v1/targets/{id}/shares/{share_id}` | revoke immediately — the link 404s on its next request |

Both `POST` and `GET` return the `token`; build the link as `/m/{token}` (prepend your origin). The token stays re-copyable — it is stored encrypted at rest (the app KEK, same as `basic_auth`/`bearer_token`); the public resolve path matches on a separate hash, so a hot link never triggers a decrypt. `token` is `null` only when a row was sealed under a KEK that is no longer configured. Two plan caps apply (columns on `plans`, overridable per-account via `plan_overrides`): `max_share_links_per_monitor` (active links on one monitor) and `max_shared_monitors` (distinct monitors with any link, counted across the account's orgs). The free plan is **1** and **2**. Exceeding either is `422 QUOTA_EXCEEDED` (the body names the `quota`). A label longer than 80 characters is `400 SHARE_LABEL_INVALID`; an `expires_at` in the past is `400 INVALID_EXPIRY`.

### Operator endpoints (variables)

A variable is a reusable named value an org's monitors reference as `{{key}}` in their HTTP request fields; the reference resolves to the value before a check runs. A secret variable's value is sealed at rest and write-only: every read path returns `value: null` for it. Endpoints are gated on `variables:read` / `variables:write` and scoped to the caller's active org. The behaviour those references unlock is documented in [Variables and secrets](variables.md).

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/api/v1/variables` | list variables (each with `used_by`, the count of referencing monitors) |
| `GET` | `/api/v1/variables/{id}` | get one variable with its `used_by` |
| `POST` | `/api/v1/variables` | create; body `{ "key", "is_secret"?, "value" }`, returns the `Variable` plus `used_by` |
| `PATCH` | `/api/v1/variables/{id}` | rotate the value; body `{ "value" }` |
| `DELETE` | `/api/v1/variables/{id}` | delete; `409 VARIABLE_IN_USE` while a monitor still references it |

A key must match `^[a-z][a-z0-9_]{0,62}$` or the create is `400 INVALID_VARIABLE_KEY`; a duplicate key in the org is `409 VARIABLE_KEY_EXISTS`. The `is_secret` flag is fixed at create. A plain variable returns its `value`; a secret returns `value: null` on every read, including the create and rotate responses. A monitor whose `{{key}}` references do not all resolve (unknown key, or a secret used in a field that forbids it) is rejected at save with `422 UNRESOLVED_VARIABLE`.

## Check specs

Tagged enum, `type` discriminator.

### HTTP

```jsonc
{
  "type": "http",
  "url": "https://example.com/healthz",
  "method": "GET",
  "timeout": 10000,                             // ms, total request budget
  "follow_redirects": false,
  "max_redirects": 0,
  "expected_status": { "kind": "exact", "value": 200 },
  "expected_body_contains": null,               // optional substring match
  "headers": {},
  "body": null,
  "verify_tls": true,
  "basic_auth": null,                           // ["user", "pass"] or null
  "bearer_token": null
}
```

The `url`, `headers`, `body`, and `expected_body_contains` fields may carry `{{key}}` references to org [variables](variables.md), resolved before the check runs. A secret variable is allowed only in a header value or the body.

#### Credential redaction

`GET`, `POST`, `PATCH`, and `bulk` responses replace populated `basic_auth` / `bearer_token` fields, and every flow-check `fill` step's `value`, with the sentinel `"***"`. A `null` field stays `null`, so clients can distinguish "auth is configured" from "no auth". When you `PATCH` a target's `check`, you must re-supply the real credential — a body whose `basic_auth`, `bearer_token`, or a flow `fill` value contains `"***"` is rejected with `400 Bad Request` (`REDACTION_SENTINEL`). If you only need to change other fields (`name`, `tags`, `enabled`, `interval`), omit `check` from the `PATCH` body. Encryption at rest is gated on [`security.credentials_kek_base64`](configuration.md); the redaction behavior applies in either mode.

`expected_status` variants:

```jsonc
{ "kind": "exact", "value": 200 }
{ "kind": "range", "value": { "min": 200, "max": 299 } }
{ "kind": "one_of", "value": [200, 204] }
```

#### Rate-limited responses

A response with `429 Too Many Requests` or `503 Service Unavailable` is recorded as `degraded`, not `down` — the upstream is telling us "I'm here, back off." The `error` field carries `rate-limited <code> (Retry-After: <value>)` when the header is present so operators can size the polling interval against what the upstream actually wants. A check that explicitly accepts 429 / 503 via `expected_status` is honored first and stays `up`.

Some third-party APIs rate-limit by source IP regardless. GitHub's unauthenticated REST API is the canonical case: 60 req/h per IP, 5 000 req/h with a token in the `Authorization` header. Poll those endpoints at ≥ 300 s, or attach the token via a header in this spec.

#### Per-host throttle

The worker side caps the number of concurrent checks one tenant can fan at the same `(host, port)` so a burst of monitors against one upstream doesn't look like a probe. When the cap is reached, the over-cap tick is **dropped**: no `CheckResult` is written, so it never counts as a failure and no alert fires — the upstream is fine, the back-pressure is operator-side. The cap is per-tenant: one customer's burst never starves another customer's monitor of the same host. Default cap is two in-flight per `(org, host, port)`; tune via `checker.per_host_max_inflight`. RDAP queries (domain expiry) carry their own per-TLD cap via `checker.rdap_max_inflight`.

### TCP

```jsonc
{ "type": "tcp", "host": "db.internal", "port": 5432, "timeout": 2000 }
```

### Ping (ICMP)

```jsonc
{ "type": "ping", "host": "gateway.internal", "timeout": 3000 }
```

Sends one ICMP echo request per resolved (SSRF-filtered) address until a reply arrives; the round-trip time is recorded as `duration_ms`. Silence for the full timeout is `down` — ICMP has no refusal signal. Self-hosters: the probe opens an unprivileged `SOCK_DGRAM` ICMP socket, so the process needs `net.ipv4.ping_group_range` to cover its GID (Docker sets this by default) or `CAP_NET_RAW`; without either, ping checks report `error` with the reason.

### DNS

```jsonc
{
  "type": "dns",
  "domain": "api.example.com",
  "record_type": "A",
  "resolver": "1.1.1.1",           // optional; omit for the process resolver
  "expected_contains": "192.0.2.1", // optional strict match
  "timeout": 3000
}
```

Resolves `domain` and reads the answers. `record_type` is one of `A`, `AAAA`,
`CNAME`, `MX`, `NS`, `TXT`, `SOA`, `PTR`, `CAA`, `SRV`. A trailing dot on the
domain is tolerated. `resolver` takes `ip` or `ip:port` (`1.1.1.1`,
`8.8.8.8:53`) to query one specific server instead of the process default.

Status: an empty answer set — including NXDOMAIN and no-records — is `down`.
With `expected_contains` set, an answer set where no value contains that
substring is also `down`, which is how a hijacked or mis-pointed record
surfaces rather than passing because *something* resolved. Without it, any
non-empty answer is `up`. A resolver failure (SERVFAIL, network error) is also
`down`; only the outer check timeout records an `error`.

The result carries `domain`, `record_type`, the `answers` list,
`expected_contains`, and `matched` so a failure shows what actually resolved.

### Heartbeat (inbound dead-man's-switch)

```jsonc
{ "type": "heartbeat", "period": 300000, "grace": 60000, "max_runtime": 900000 }
```

Reverses the direction: instead of the platform probing your system, your system pings the platform. Creating a heartbeat monitor mints a capability URL, returned by `GET /api/v1/targets/{id}/heartbeat` (member-level `targets:write`, since the ping URL is itself a write capability) as `{ "ping_url": "...", "pending": false, "first_ping_at": "...", "created_at": "...", "last_ping_at": "...", "last_start_at": "...", "last_fail_at": "...", "last_exit_code": 137, "last_failure_output": "...", "declared_period_secs": 600, "observed_period_secs": 4920, "cadence_advice": { "kind": "too_tight", "suggested_period_secs": 5400 } }`; call it (GET or POST, no auth, e.g. `curl -fsS $URL` at the end of a cron job) on every run. The scheduled evaluation compares the age of the last success against `period + grace` (both in milliseconds): inside that window the monitor is `up`, past it the monitor goes `down` through the normal incident pipeline. A monitor that has never been pinged is `pending` (`first_ping_at` is `null`): it is not evaluated at all, writes no check results and opens no incidents, so the wait between creating it and wiring up the job is free. The first ping of any signal ends that, including a `fail`, which alerts straight away. A monitor that has pinged before and is then re-enabled gets a full `period + grace` from the resume.

**Signals.** A path segment after the token says what the ping means, so a job can report more than "still alive":

| Call | Meaning |
|---|---|
| `$URL` or `$URL/success` | the run finished cleanly; this is what clears a failure and advances the window |
| `$URL/start` | the run has begun. Does not advance the window: it opens a run, it does not report one |
| `$URL/fail` | the job knows it failed. Goes `down` immediately instead of waiting out `period + grace` |
| `$URL/$?` | the shell's exit status, 0–255. `0` is a success, anything else a failure carrying that code |

An unrecognised segment is a `404`, never a success, so a typo cannot keep a broken job green. Pairing `/start` with a finish gives that run a duration; `max_runtime` (optional, 60 s to 30 d) then bounds it, so a job that started and never finished opens an incident without waiting out the whole period. Without `/start` there is no run to bound and `max_runtime` does nothing.

A POST body is kept as that run's output, truncated to 4 KiB and dropped on a shorter clock than the ping itself (see [data retention](hosted/data-retention.md)). Oversized bodies are truncated, never refused — a `413` on a success ping would page you for a job that ran fine. The ping counts as soon as it resolves, before the body is read, so a body the server stops reading (past 256 KiB, or 10 s of trickle) costs you the output, never the ping.

**Cadence advice.** `observed_period_secs` is the median gap between *successes* over the last 14 days — the only evidence of how often the job really runs, since a ping arriving inside its window changes no check result and so leaves no other trace. It is judged against `period + grace`, the point at which the monitor actually goes down, not against the declared period alone: grace exists to absorb jitter, so a job living inside it is not late. When the two disagree materially, `cadence_advice` names the direction and what to use instead: `too_tight` (the real cadence is slower than `period + grace`, so ordinary runs page you) or `too_loose` (`period + grace` is at least 4× the real cadence, so a dead job goes unnoticed far longer than it needs to). `observed_period_secs` is `null` until a second success gives it a gap to measure; `cadence_advice` is absent from the response until there are at least five. `/start` pings are excluded from both — a start says a run began, not that the schedule came round.

**Rotation.** `POST /api/v1/targets/{id}/heartbeat/rotate` (member-level `targets:write`) mints a replacement ping URL on the same monitor: incidents, history, share links and status-page bindings are untouched, and the silence clock is not re-armed. Its JSON body is required; `{}` asks for the ordinary rotation. By default the superseded URL keeps working for 24 hours, since a URL that dies instantly does not alert: the job goes quiet and pages a full `period + grace` later. During that overlap the response and `GET .../heartbeat` carry `previous_url_expires_at` and `previous_url_last_used_at`, so you can see whether anything still calls the old URL; pings on it count normally. `DELETE /api/v1/targets/{id}/heartbeat/previous` ends the overlap early (idempotent, 204 either way). For a leaked URL pass `{"revoke_previous_immediately": true}` and the old URL 404s from the same commit, indistinguishable from an unknown token. `rotated_at` records the last rotation.

Constraints: `period` runs from 60 s to 30 d, `grace` from 0 to 30 d, `max_runtime` from 60 s to 30 d, and the monitor's `interval` (evaluation cadence) has a 60 s kind floor. Heartbeats never run on regional probes and reject `test`/`check-now` (`HEARTBEAT_NOT_PROBEABLE`) and region assignment, since there is nothing to probe. Unknown ping tokens 404 uniformly; per-token ping ingest is rate-limited (429 with `Retry-After`) at 1/s sustained with a burst of 10, which is shared across all of a token's signals.

Provisioning: a single create mints the ping URL immediately; bulk-created heartbeat monitors get theirs within one scheduler refresh (~30 s). Pick `grace` with your deployment in mind: a ping sent while the control plane itself is unreachable is lost, so on single-node self-hosts keep `grace` comfortably above your restart window (the hosted service deploys blue/green, so ping ingest stays up).

### TLS certificate expiry

```jsonc
{
  "type": "tls_cert",
  "host": "example.com",
  "port": 443,
  "server_name": null,         // optional SNI override; defaults to `host`
  "warn_days": 14,
  "critical_days": 7,
  "timeout": 10000
}
```

Opens a TCP connection, performs a TLS handshake against the host (accepting any presented chain so that expired or self-signed certs can still be inspected), and parses the leaf certificate's `notAfter`. Status mapping:

- `days_remaining < 0` (expired) → `down`
- `days_remaining < critical_days` → `down`
- `days_remaining < warn_days` → `degraded`
- otherwise → `up`

`error` carries a JSON document with `days_remaining`, `not_after`, `subject_common_name`, `issuer_common_name`. A handshake failure (plain-TCP host, network error) returns `error` status with the underlying message. `warn_days` must be strictly greater than `critical_days`. Floor is `interval >= 3600` (enforced); a monitor created in the form opens at `43200` (twice daily), which leaves room for a short-lived certificate whose `warn_days` is only a day or two.

### Domain expiration

```jsonc
{
  "type": "domain_expiry",
  "domain": "example.com",
  "warn_days": 30,
  "critical_days": 7,
  "timeout": 10000
}
```

Queries the [IANA RDAP bootstrap registry](https://data.iana.org/rdap/dns.json) to find the authoritative RDAP server for the domain's TLD, then fetches `/domain/<domain>` and reads the `events[?eventAction == "expiration"]` entry. Status mapping is the same as TLS cert: `< critical_days` → `down`, `< warn_days` → `degraded`, else `up`. Non-`up` results carry a JSON `error` body with `domain`, `days_remaining`, `expiration_date`, and (when present) `registrar`.

The bootstrap registry is fetched lazily on the first lookup and cached for the lifetime of the process. The SSRF guard does not apply — the check's network destination is an IANA-published RDAP server, not the user-supplied domain. Floor is `interval >= 43200` (enforced, because RDAP servers rate-limit by source address); default for a new monitor is `86400` (daily). `warn_days` must be strictly greater than `critical_days`.

### Flow (browser login/transaction)

```jsonc
{
  "type": "flow",
  "start_url": "https://app.example.com/login",
  "steps": [
    { "op": "fill",        "selector": "#username", "value": "monitor@example.com" },
    { "op": "fill",        "selector": "#password", "value": "{{login_password}}" },
    { "op": "click",       "selector": "button[type=submit]" },
    { "op": "assert_url",  "contains": "/dashboard" },
    { "op": "assert_text", "selector": null, "contains": "Signed in" }
  ],
  "timeout": 30000,        // whole-run budget, 1000..=120000 ms
  "step_timeout": 5000,    // per-step wait for a selector, 100..=60000 ms
  "verify_tls": true
}
```

Drives a real headless browser from `start_url` through each step in order, so it verifies a login *session* (form fill, submit, authenticated page) rather than just that an endpoint responds. Steps run top to bottom: `goto` navigates, `fill` types into a selector, `click` clicks, `wait_for` waits for a selector to appear, `assert_text` requires a substring (optionally scoped to a selector's text via `selector`, or page-wide with `selector: null`), and `assert_url` requires the current URL to contain a substring. At least one `assert_*` step is required so a broken login fails the check instead of passing silently. A flow holds up to 30 steps.

Use a dedicated low-privilege test account, never a real or admin credential: the flow stores that credential and it runs on the probing agent. Put passwords in an org secret and reference it as `{{name}}` in a `fill` value (resolved at probe time, so only the token is stored); an inline literal is kept as typed. Every navigation URL is SSRF-filtered like other checks.

`timeout` caps the whole run and `step_timeout` caps each selector wait. `verify_tls` is on by default; turn it off only for an internal endpoint with a self-signed certificate. Floor is `interval >= 300` (enforced). A flow runs only where a browser engine is available, so its regions are clamped to the flow-capable set rather than every region. The number of flow monitors an org may create is capped per plan. A plan whose cap is 0 has the kind switched off entirely, and a create returns `403 FLOW_CHECKS_DISABLED` rather than a quota error; that is where a self-hosted install starts.

## Target payload

```jsonc
{
  "name": "internal-api",
  "check": { /* check spec */ },
  "interval": 60,             // seconds between ticks; effective floor is
                              // max(plan.min_check_interval_secs, kind_min).
                              // kind_min is 10 for http/tcp/ping/dns, 60 for
                              // heartbeat, 300 for flow, and 3600 for
                              // tls_cert/domain_expiry. Free plan floor is 180;
                              // a DB-less deployment has no plans table, so
                              // only kind_min and the schema's own
                              // `interval_secs >= 10` check apply.
  "enabled": true,
  "tags": ["prod", "tier1"], // at most 50, each at most 50 characters, no blank
                             // and no control or invisible characters. Trimmed
                             // and de-duplicated before the count is applied.
  "regions": ["eu-frankfurt"], // optional; ids from GET /api/v1/regions. Omit to
                             // take the default-selected set under the plan's
                             // region cap. Every id must be enabled, the set
                             // must fit the cap, and a heartbeat may name none.
  "group_name": "API",       // optional, at most 50 characters; null clears
  "owner_user_id": "…",      // optional member of the org; null clears
  "alerts": [ /* optional, see below */ ],
  "alert_confirmations": 2,   // see "Alert config" below, with
  "notify_recovery": true     // renotify_interval_secs and region_policy
}
```

Server returns the full `Target` including `id` (UUIDv7), `created_at`, `updated_at`, and `write_source`. The assigned regions are read back from `GET /api/v1/targets/{id}/regions` and changed with `PUT` on the same path; a `regions` array that names an unknown or disabled id, or any id on a heartbeat, is `422 REGION_INVALID`, and one wider than the plan allows is `422 QUOTA_EXCEEDED`. A `POST /api/v1/targets/bulk` item takes the same field; an item without it shares the default set.

A target `POST` or `PATCH` body may carry only the keys in this section.
Anything else, a misspelt key or a read-only field echoed back from a `GET`
such as `id` or `write_source`, is `422 INVALID_JSON` naming the key, so a
setting under the wrong name fails loudly rather than silently leaving the
default in place. A body that is not JSON at all is `400 INVALID_JSON`; one
sent without `Content-Type: application/json` is `415 INVALID_CONTENT_TYPE`.
The other resources still ignore a key they do not know.

`write_source` is a read-only field recording where the resource was last
written from: `ui`, `api`, or `terraform` (decided server-side from the
request, never the body). It also appears on
notification channels and maintenance windows, and drives the "managed by"
badge in the web UI. A write through any endpoint restamps it, so it reflects
the most recent author.

### Alert config

`alerts` is an optional array of channel bindings. Each binding is just a
reference to a notification channel (see
[Notification channels](#notification-channels)); the firing policy lives on
the monitor itself. An empty/omitted array disables channel alerting for that
target (incidents still open and show on status pages).

```jsonc
"alerts": [
  { "channel_id": "0192a1ce-0000-7000-8000-000000000001" },
  { "channel_id": "0192a1ce-0000-7000-8000-000000000002" }
],
"alert_confirmations": 3,
"notify_recovery": true,
"renotify_interval_secs": 3600,
"region_policy": "majority"
```

- `channel_id` — id of a notification channel owned by the **same org**. A binding to an unknown or another tenant's channel is rejected. It is the binding's only key; the firing policy is the monitor's `alert_confirmations` and `notify_recovery` below, and a per-binding key is `422`.
- `alert_confirmations` — consecutive failing checks before an incident opens (and the same number of passing checks before it closes, which damps flapping). Default `2`, must be `>= 1`.
- `notify_recovery` — when `true` (default), the recovery is announced to the monitor's channels. When `false`, recovery is silent.
- `renotify_interval_secs` — seconds before the first reminder while an outage stays unacknowledged. Each further reminder doubles the gap, capped at a day, so a long outage nobody answers decays to a daily nudge. An interval already longer than a day keeps its own cadence. `0` disables reminders; otherwise must be `>= 60`. Default `3600`. Acknowledging or resolving the incident stops the reminders.
- `region_policy` — how many probe regions must agree the target is down before an incident opens: `"any"`, `"majority"` (default), `"all"`, or `{ "count": N }`.

Notifications are driven by the incident engine: one notification per
incident open (then backing-off reminders per `renotify_interval_secs`), one on recovery.
Failed deliveries retry on exponential backoff and dead-letter after the
attempt cap; per-incident delivery state is visible at
`GET /api/v1/incidents/{id}/notifications`.

### Alert validation errors

`POST` and `PATCH` return `400 Bad Request` (`INVALID_ALERT_CONFIG`) for:

- a duplicate `channel_id` in the array
- `notification channel <id> does not exist` — unknown id, or one owned by another org
- `alert_confirmations must be >= 1`
- `renotify_interval_secs must be 0 (off) or at least 60`

A `region_policy` of `{ "count": N }` where `N` is `0` or exceeds the
available regions is `422 INVALID_REGION_POLICY`. A count within the catalog but
wider than the regions one monitor is assigned is accepted and clamped to the
regions that report, since a monitor's region set can be narrowed at any time
after it is created.

### Validation errors

`POST` and `PUT` return `400 Bad Request` for:

- Unsupported URL scheme (`url scheme '...' not allowed` — only `http` and `https`)
- Missing URL host, empty TCP host, or TCP/TLS port `0`
- `tls_cert warn_days must be > critical_days` (each must also be in `1..=365`)
- `domain_expiry domain must be of the form 'name.tld'` (no dot, or an empty label on either side of it)
- `domain_expiry warn_days must be > critical_days` (each must also be in `1..=365`)
- `domain_expiry` create is refused when the domain's TLD registry publishes no expiry data: the check could never succeed
- **SSRF guard** — `target address ... is in a blocked range`. Triggered when the URL or TCP host is an IP literal that resolves to loopback / private / link-local / reserved space (see [Configuration → `security.allow_private_targets`](configuration.md)). Hostname literals are checked again at connect time after DNS resolution, so DNS rebinding cannot bypass the guard.
- **Redaction sentinel** — `basic_auth contains redaction sentinel — re-supply the real credential` or the equivalent for `bearer_token`. Rejected to prevent a `GET` → `PATCH` round-trip from silently overwriting the stored credential with `"***"`.
- **TLS verification + credentials** — `verify_tls = false cannot be combined with basic_auth or bearer_token over https`. When verification is disabled any host presenting a forged certificate can collect the stored credential on every check interval. Set `verify_tls = true` (recommended) or remove the credential from the target.

## Notification channels

Per-org delivery destinations that targets bind to via their `alerts` array.
Org scoping is implicit in the caller's authenticated context — one tenant can
never read, mutate, or test another's channels.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/api/v1/notification-channels` | Create a channel (201 + `Location`) |
| `GET` | `/api/v1/notification-channels` | List the org's channels |
| `GET` | `/api/v1/notification-channels/{id}` | Get one |
| `PATCH` | `/api/v1/notification-channels/{id}` | Partial update |
| `DELETE` | `/api/v1/notification-channels/{id}` | Delete (204); also removes the channel's alert bindings from every monitor |
| `POST` | `/api/v1/notification-channels/test` | Test an **unsaved** transport config |
| `POST` | `/api/v1/notification-channels/{id}/test` | Send a synthetic test alert through a saved channel |
| `POST` | `/api/v1/notification-channels/{id}/resend-verification` | Resend the verification mail for an unverified email channel |

```jsonc
{
  "name": "Ops Slack",
  "enabled": true,
  "config": { "type": "slack", "webhook_url": "https://hooks.slack.com/services/T/B/XXXX" },
  "auto_bind_tags": ["db"]
}
```

`auto_bind_tags` is the channel's tag rule: on top of the monitors bound to it, the channel pages any monitor carrying at least one of these tags, resolved when the alert fires. Optional on create, replaced whole on `PATCH`, and `[]` clears it. Tags obey the same rules as monitor tags, except that matching ignores case: a rule reading `DB` covers a monitor tagged `db`, and two spellings of one tag are stored once. Tag *filters* elsewhere in the API stay exact.

`config` is `type`-tagged. Supported transports:

- `slack` — `{ "type": "slack", "webhook_url": "https://…", "mention": "@here S01ABC234" }` (incoming webhook; posts `{ "text": "…" }`. `mention` is optional: `@here`, `@channel`, user-group ids (`S…`) or member ids (`U…` / `W…`), space or comma separated, up to 5. It leads the text on opened/reopened/escalated/no-data only, is dropped for `@here`/`@channel` on test sends, and is not a secret, so it is returned unmasked)
- `discord` — `{ "type": "discord", "webhook_url": "https://discord.com/api/webhooks/…", "mention": "@here &123456789012345678" }` (channel webhook; posts an embed with `?wait=true` so delivery failures surface synchronously. `mention` is optional: `@everyone`, `@here`, role ids (`&123…`) or member ids (`123…`), space or comma separated, up to 5. A role id and a member id share one shape, so a role must carry the leading `&` or it resolves to nobody. Discord resolves no mention inside an embed, so it rides the message content, and `allowed_mentions` names exactly those targets and nothing else. Same lifecycle as Slack: opened/reopened/escalated/no-data only, `@everyone`/`@here` dropped on test sends, returned unmasked because it is not a secret)
- `mattermost` — `{ "type": "mattermost", "webhook_url": "https://mattermost.example.com/hooks/…", "mention": "@here oncall-sre" }` (incoming webhook; posts one message attachment with a coloured bar, a linked title and a field table. Every install serves its own host, Cloud and on-prem alike, so there is nothing to host-pin: the URL is checked for `https` and for the `/hooks/<key>` segment every incoming webhook ends in, so a REST-API URL is refused, and a subpath install behind a reverse proxy is fine. The whole URL is a secret — anyone holding the key can post to the server — so it is returned masked. `mention` is optional: `@channel`, `@here`, `@all`, or a username or group name (up to 64 of `a-z`, `0-9`, `.`, `-`, `_` — the server's own rule, not the tighter one the signup form enforces, so a directory-synced handle still fits), space or comma separated, up to 5, lowercased on the way in because Mattermost handles are lowercase. Mattermost resolves a handle at delivery, so one that belongs to nobody is only caught then. It rides the attachment's pretext, the one field the alert lets carry a ping deliberately: Mattermost scans an attachment's title, body and fields for mentions too, so nothing else in the message can widen who gets woken. The error sits in a code fence, which the scanner skips; the monitor name heads the card with its `@` signs defused by a zero-width joiner, so it reads as itself and resolves to nobody; and the push line rides `fallback`, which the scanner never reads at all. Same lifecycle as Slack: opened/reopened/escalated/no-data only, `@channel`/`@here`/`@all` dropped on test sends, returned unmasked because it is not a secret)
- `msteams` — `{ "type": "msteams", "webhook_url": "https://….logic.azure.com/…" }` (Teams Workflows webhook; posts an Adaptive Card. Retired O365 connector URLs are not accepted)
- `google_chat` — `{ "type": "google_chat", "webhook_url": "https://chat.googleapis.com/v1/spaces/…" }` (space webhook; posts `{ "text": "…" }`, capped at 4096 chars)
- `webhook` — `{ "type": "webhook", "url": "https://…", "headers": { … }, "secret": "…" }` (POSTs the alert JSON; optional custom headers; optional signing secret, see below). The escape hatch: no host restrictions, for services the named kinds don't cover
- `telegram` — `{ "type": "telegram", "bot_token": "…", "chat_id": "…" }` (bring-your-own bot)
- `telegram_app` — `{ "type": "telegram_app", "chat_id": "…", "chat_title": "…" }` — linked through the platform's central bot. **Not creatable from request bodies**: a `POST`/`PATCH`/test carrying this kind returns `422 CHANNEL_KIND_MANAGED` (the chat id rides the operator bot's credentials, so accepting one would let any caller page an arbitrary chat). Channels of this kind are created only by the link-code flow below.
- `whatsapp` — `{ "type": "whatsapp", "access_token": "…", "phone_number_id": "…", "to": "…", "template_name": "…", "language_code": "en" }` (Business Cloud API; `language_code` optional, default `en`)
- `whatsapp_app` — `{ "type": "whatsapp_app", "phone": "…", "profile_name": "…" }` — linked through the platform's WhatsApp number. **Not creatable from request bodies** (`422 CHANNEL_KIND_MANAGED`, same rationale as `telegram_app`); created only by the WhatsApp link-code flow below.
- `pagerduty` — `{ "type": "pagerduty", "routing_key": "…" }` (the 32-character Events API v2 integration key of a PagerDuty service). The only transport that drives the destination's own incident lifecycle: opens/reopens/escalations send `trigger` and resolution sends `resolve`, all correlated by `dedup_key` = the incident id, so one uptimepage incident maps to exactly one PagerDuty alert that opens and closes with it. Severity maps Critical→`critical`, Major→`error`, Minor→`warning`. A test send fires a `trigger`+`resolve` pair on a throwaway dedup key and never leaves an open PagerDuty incident
- `ntfy` — `{ "type": "ntfy", "server_url": "https://ntfy.sh", "topic": "…", "access_token": "tk_…" }` (JSON publish to the server root; `server_url` optional, defaults to ntfy.sh, must be the bare server root; `access_token` optional, sent as a Bearer token). High-urgency opens publish at priority 4, the rest at 3; resolves tag `white_check_mark`, opens `rotating_light`. On ntfy.sh an unprotected topic's name is its only access control
- `gotify` — `{ "type": "gotify", "server_url": "https://push.example.com", "token": "…" }` (JSON publish to `{server_url}/message`; `server_url` is the base URL of your own server, path included when it sits under one, so pass the base and not the publish endpoint; `token` is an application token, sent as `X-Gotify-Key`). High-urgency opens publish at priority 8 (sound and vibration on the Android client), other opens at 5 (sound), resolves at 3 (notification bar only); the incident link rides along as the notification's click URL
- `pushover` — `{ "type": "pushover", "token": "…", "user": "…", "device": "…", "emergency": false }` (30-character application token and user/group key, both treated as secrets; `device` optional). High-urgency alerts go out at priority 1 (bypasses quiet hours), low at 0, resolves at −1 (no sound). With `emergency: true`, a live high-urgency open instead sends priority 2: Pushover re-alerts every 60 s for up to 3600 s until acknowledged, and the repeat is cancelled the moment the incident resolves. Low-urgency alerts and resolves are unaffected by the flag
- `sms` — `{ "type": "sms", "provider": "twilio", "to": "+15551234567", "from": "…", … }` — bring-your-own SMS gateway; one text message per alert, body trimmed to a few segments to bound per-segment cost. `to` is E.164; `from` is an E.164 number or sender id. The provider-specific credentials are: `twilio` → `account_sid` + `auth_token`; `telnyx` → `api_key` (+ optional `messaging_profile_id`); `vonage` → `api_key` + `api_secret`; `plivo` → `auth_id` + `auth_token`; `sinch` → `service_plan_id` + `api_token` + `region` (`us`/`eu`/`au`/`br`/`ca`, default `us`). Only the gateway secret is treated as a secret (Twilio/Plivo `auth_token`, Telnyx `api_key`, Vonage `api_secret`, Sinch `api_token`); account identifiers stay visible
- `email` — `{ "type": "email", "to": "oncall@example.com" }` — one address per channel, stored lowercased, delivered through the platform's transactional sender. **Verification-gated**: the channel is created unverified and a mail with a single-use 24 h link is sent to the address; until the link is confirmed every delivery (incident page or test send) fails with `email address not verified`. Replacing the config resets the gate and re-sends the mail. `POST /api/v1/notification-channels/{id}/resend-verification` re-sends it (capped per channel and per org per day — `422 CHANNEL_VERIFICATION_LIMIT`; on a non-email channel — `422 CHANNEL_NOT_VERIFIABLE`); a test against an unverified or unsaved email config is `422 CHANNEL_UNVERIFIED`.

**Webhook signing.** When a `webhook` channel carries a `secret` (≥ 16
characters), every delivery is signed: the request includes
`X-Uptimepage-Timestamp` (unix seconds) and
`X-Uptimepage-Signature: sha256=<hex>`, where the hex is
HMAC-SHA256(`secret`, `"{timestamp}.{body}"`) over the exact bytes sent.
Receivers should recompute the digest and reject stale timestamps (e.g.
older than 5 minutes) to block replays. Channels without a secret deliver
unsigned.

**Webhook payload.** The body is the incident notice serialized as-is, no transport-specific wrapping:

```jsonc
{
  "incident_id": "0192a1ce-89b1-7c3a-9e21-4b6e2a9d9f10",
  "reason": "opened",
  "monitor_name": "api-prod",
  "title": null,
  "severity": "major",
  "urgency": "high",
  "started_at": "2026-05-13T11:30:00Z",
  "ended_at": null,
  "error_sample": "connection refused",
  "regions_down": ["eu-west", "us-east"],
  "regions_up": ["ap-south"],
  "url": "https://app.uptimepage.dev/i/0192a1ce-89b1-7c3a-9e21-4b6e2a9d9f10"
}
```

| Field | Type | Notes |
|---|---|---|
| `incident_id` | string (UUID) | |
| `reason` | string | see the enum values below |
| `monitor_name` | string \| null | `null` for a manual incident not tied to a monitor |
| `title` | string \| null | set on a manual incident that has no monitor |
| `severity` | string | `minor` \| `major` \| `critical` |
| `urgency` | string | `high` (pages) \| `low` (records, doesn't page) |
| `started_at` | string (RFC 3339) | |
| `ended_at` | string (RFC 3339) \| null | set only once the incident resolves |
| `error_sample` | string \| null | |
| `regions_down` / `regions_up` | string[] | regions that had confirmed the failure when the notification was built, and those that had not yet. `regions_up` is not a healthy verdict: with a majority quorum the last region is usually one check behind, and it lands there until it confirms. On the stored incident (`GET /api/v1/incidents/{id}`) a region moves to `regions_down` once it confirms and stays there, so the incident records the worst the outage was; a region that recovers before the close is not moved back. A notification carries the split as it stood when it was built. Empty on both sides for a single-region monitor |
| `url` | string \| null | deep link to the incident detail page, when a base URL is configured |
| `note` | string | present only when there is something to say about the alert stream itself, such as a flapping monitor whose repeat alerts are being held |

`reason` is one of: `opened`, `escalated`, `reopened`, `resolved`, `reminder` (the incident is still open and unacknowledged), `nodata` (the monitor's probes went silent, no incident, orthogonal to up/down), `dataresumed` (probing recovered after a `nodata` notice).

**WhatsApp templates.** Create a one-parameter utility template (body
`{{1}}`) in the WhatsApp Business Manager and set `template_name` (plus
`language_code`, which must match the template's exact language — `en`
and `en_US` are distinct). The alert text is sent as that single
parameter, collapsed to one line. A template is required: WhatsApp
accepts free-form text only within 24 hours of the recipient's last
message, and out-of-window sends are accepted by the API yet dropped
asynchronously — a silent-loss mode an alerting channel must not have.

Behaviour:

- **Secrets sealed at rest** with the credentials KEK; **never echoed back**. Every read path masks secret-bearing fields with `***` (the webhook URL is masked whole — it can carry a token; header *names* and `chat_id` are kept so the UI stays useful).
- **Redaction-sentinel guard**: submitting a `config` that still contains `***` returns `400 REDACTION_SENTINEL`. Omit `config` on `PATCH` to keep the stored secret unchanged.
- **Normalization**: config values are cleaned before they are checked, so a value pasted with the whitespace a clipboard carries is accepted rather than failing a length or shape rule. Addresses are lowercased, and the fields that can only be a phone number (`sms` `to`, `whatsapp` `to`, WhatsApp-app `phone`) keep their digits and leading `+` while losing the spaces, dashes, dots and brackets a number is displayed with. An SMS `from` is left exactly as sent, since a sender id may contain a dash.
- **Validation** (`400`): every webhook URL must be `https`; the provider-branded kinds are additionally host-pinned (`discord` → `discord.com`/`discordapp.com` with an `/api/webhooks/` path, `msteams` → `*.logic.azure.com`/`*.powerplatform.com`, `google_chat` → `chat.googleapis.com`) and a URL elsewhere is rejected with a hint to use the generic `webhook` kind; `mattermost` has no fixed host — Cloud and on-prem installs each serve their own — so it is path-pinned instead: the URL must end in `/hooks/<key>`; `telegram` requires non-empty `bot_token` and `chat_id`; `whatsapp` requires `access_token`, a numeric `phone_number_id`, an international-format `to`, and a `template_name` (lowercase/digits/underscore); `email` requires a single-address `to`, lowercased on the way in; `pagerduty` requires a 32-char alphanumeric `routing_key`; `ntfy` requires an https root-only `server_url` and a 1–64 char `topic` (letters/digits/`_`/`-`); `gotify` requires an https `server_url` without query, fragment or inline credentials and a 1–200 char whitespace-free `token`; `pushover` requires 30-char alphanumeric `token` and `user`; `sms` requires an E.164 `to`, a `from`, and the selected provider's credentials (Twilio `account_sid` is `AC` + 32 hex; Plivo `auth_id` and Sinch `service_plan_id` are alphanumeric; Sinch `region` is one of `us`/`eu`/`au`/`br`/`ca`); channel `name` is required and ≤ 100 chars.
- **Destination deny-list**: the customer-controlled outbound URL (`slack`/`discord`/`msteams`/`google_chat`/`mattermost`/`webhook`/`ntfy`'s and `gotify`'s `server_url`) is checked against the platform's abuse deny-list on create, update, and both test endpoints — a match is rejected (`ABUSE_BLOCKED` / `DOMAIN_DENYLISTED`). `telegram`/`whatsapp`/`email`/`pagerduty`/`pushover`/`sms` deliver to fixed vendor endpoints.
- **Quota**: capped per org by the plan's `max_notification_channels` (atomic, advisory-locked). A duplicate name within the org is `422 CHANNEL_NAME_TAKEN`; the cap is `422 CHANNEL_QUOTA_EXCEEDED`.
- **Test sends** deliver one clearly-labelled synthetic alert. The per-channel form tests the stored config (works on a disabled channel too); the collection-level `POST …/test` takes `{ "config": { … } }` in the body, validates it exactly as create would, and persists nothing — the UI uses it for "test now" before a channel is saved. A transport failure is `422 CHANNEL_TEST_FAILED`. Both count against the `test_now` rate-limit bucket.
- **Platform disables**: when a linked Telegram chat unlinks from its side (the bot is removed, or the chat sends `/stop`), every channel linked to that chat is disabled with a `disabled_reason` the UI shows. Re-enabling the channel clears the note.
- **Not delivering**: a channel whose deliveries keep using up every retry is flagged in the UI and the org's owners are mailed once about it. It is not disabled and keeps being paged, because a silent endpoint costs a few wasted requests per incident while switching one off costs the next outage. Any delivery that lands clears the flag, as does turning the channel off and back on; a per-channel test counts only while the channel is enabled. The response carries `consecutive_failures`, `failing_since` and `last_delivered_at`; the flag itself is `consecutive_failures` against the deployment's `escalation.channel_failure_limit`, so it is not a field. An email channel still awaiting verification is left out: it fails every delivery by design and already reads as unverified.

### Telegram one-tap linking

Deployments running the central bot expose a link-code flow (absent — `404 TELEGRAM_LINK_NOT_FOUND` — otherwise):

- `POST /api/v1/notification-channels/telegram-link` (`channels:write`) with an optional `{ "name": "…" }` hint mints a single-use code (15-minute expiry, capped outstanding codes per org → `422 TELEGRAM_LINK_LIMIT`). The response carries the raw `code` (shown once, only its hash is stored), a `deep_link` (`t.me/<bot>?start=<code>`, private chat) and a `group_deep_link` (`?startgroup=<code>`, picks a group). The same code works for either destination.
- Sending the code to the bot (tap **Start**, or `/link <code>` in a group) creates the `telegram_app` channel for the minting org. The org is resolved only from the code — never from the Telegram payload.
- `GET /api/v1/notification-channels/telegram-link/{id}` (`channels:read`) polls the code: `pending`, `consumed` (with `channel_id`), or `expired`.
- Unlink = delete the channel; deleting the last channel linked to a group also walks the bot out of that group. From the chat side, `/stop` or removing the bot disables the channel (see platform disables above).

### WhatsApp one-tap linking

Deployments with the operator WhatsApp number enabled expose the same flow (absent — `404 WHATSAPP_LINK_NOT_FOUND` — otherwise):

- `POST /api/v1/notification-channels/whatsapp-link` (`channels:write`) with an optional `{ "name": "…" }` hint mints a single-use code (15-minute expiry, capped per org → `422 WHATSAPP_LINK_LIMIT`). The response carries the raw `code` and a `deep_link` (`wa.me/<number>?text=<code>`) that opens WhatsApp with the code prefilled.
- Sending the prefilled message creates the `whatsapp_app` channel for the minting org, bound to the sender's number. The org is resolved only from the code — never from the webhook payload.
- `GET /api/v1/notification-channels/whatsapp-link/{id}` (`channels:read`) polls the code: `pending`, `consumed` (with `channel_id`), or `expired`.
- Unlink = delete the channel; from the phone side, sending `stop` disables every channel bound to the number (platform disable, reason shown in the UI).

### Delegation links

The person who owns the Slack workspace / Telegram group / inbox often isn't the person configuring monitors — a delegation link hands off just the connect step.

- `POST /api/v1/notification-channels/delegate` (`channels:write`) with optional `{ "name": "…", "kind": "…" }` hints mints a single-use `/c/<code>` URL (7-day expiry, capped outstanding links per org → `422 DELEGATE_LINK_LIMIT`; unknown `kind` → `400 DELEGATE_KIND_INVALID`). Only the code's hash is stored.
- `GET /c/<code>` is public and chrome-less: it offers exactly the connect-capable transports of the deployment — the telegram one-tap link + QR (the delegation code doubles as the `t.me` start payload), "add to Slack" / "add to Discord" when the operator OAuth apps are configured, and a manual webhook/address form. The link can create **one** channel in the inviting org and read nothing; expired, revoked, and spent codes all render the same 404 page. Every delegated create lands in the org audit log.
- `GET /api/v1/notification-channels/delegate` (`channels:read`) lists the org's links (`pending` / `consumed` / `expired`); `DELETE /api/v1/notification-channels/delegate/{id}` (`channels:write`) revokes an unconsumed one (revoked links read as expired).

## Rate limiting

`/api/v1/*` is rate-limited per authenticated subject — by `(org, category)` and by `(user, category)`, whichever trips first — with the per-minute budgets taken from the org's plan. Categories: `api_writes` (POST/PATCH/DELETE), `api_reads` (GET/HEAD/OPTIONS), `bulk_ops` (`/bulk*`), `test_now` (`/test`), `check_now` (`/check-now`), and `support` (`/support`), the one category with a fixed ceiling on every plan. Exceeding a budget returns `429 Too Many Requests` with a `Retry-After` header (seconds until the next token) and `code: RATE_LIMITED`. `/healthz` and `/readyz` are never throttled. Unauthenticated and per-IP limiting is the reverse proxy's job (see [Deployment](deployment.md)). Full model: [Quotas & rate limits](quotas.md).

## CORS

Disabled by default. When [`api.cors.enabled = true`](configuration.md), `/api/v1/*` answers preflight `OPTIONS` with `Access-Control-Allow-Origin` (matching `allowed_origins` or `*` when `allow_any_origin = true`), `Access-Control-Allow-Methods` (the configured list), and `Access-Control-Allow-Headers: content-type`. `/healthz` and `/readyz` carry no CORS headers regardless.

## Error envelope

Every 4xx and 5xx response uses one wire shape:

```jsonc
{
  "error": {
    "code": "INVALID_URL_SCHEME",
    "message": "url scheme 'ftp' not allowed",
    "field": "check.url",
    "details": null,
    "trace_id": null
  }
}
```

- `code` is stable, machine-readable, UPPER_SNAKE_CASE. Never repurposed once published.
- `field` is a JSON pointer to the offending input for 400s; `null` for non-field errors.
- `details` carries optional structured context (e.g., `{ "range": "127.0.0.0/8" }` for SSRF rejections).
- `trace_id` is the W3C `traceparent` when tracing is enabled.

Common codes: `INVALID_URL_SCHEME`, `INVALID_URL_FORMAT`, `SSRF_BLOCKED`, `INVALID_INTERVAL`, `INVALID_TIMEOUT`, `INVALID_TCP_PORT`, `INVALID_TCP_HOST`, `INVALID_PING_HOST`, `INVALID_HEARTBEAT_PARAMS`, `HEARTBEAT_NOT_PROBEABLE`, `INVALID_STATUS_RANGE`, `INVALID_TLS_CERT_PARAMS`, `INVALID_DOMAIN_PARAMS`, `INVALID_FLOW_PARAMS`, `FLOW_CHECKS_DISABLED`, `NO_FLOW_CAPABLE_AGENT`, `SMS_ALERTS_DISABLED`, `INVALID_TLS_CRED_COMBO`, `INVALID_ALERT_CONFIG`, `REDACTION_SENTINEL`, `BULK_EMPTY`, `BULK_TOO_LARGE`, `BAD_TIME_RANGE`, `TARGET_NOT_FOUND`, `CHANNEL_NOT_FOUND`, `CHANNEL_NAME_TAKEN`, `CHANNEL_NAME_INVALID`, `CHANNEL_QUOTA_EXCEEDED`, `INVALID_CHANNEL_CONFIG`, `CHANNEL_TEST_FAILED`, `CIRCUIT_OPEN`, `DEPENDENCY_DOWN`, `INTERNAL`.

### Quota, rate-limit and abuse codes

| Code | HTTP | Meaning |
|---|---|---|
| `QUOTA_EXCEEDED` | 422 | A plan quota would be exceeded. `details` carries `quota` (e.g. `max_targets`, `max_members`, `max_public_components`), `current`, `limit`, `plan`. |
| `MIN_CHECK_INTERVAL` | 422 | Requested check interval is below the effective floor (`max(plan.min_check_interval_secs, kind_min)`), where `kind_min` is 43200 for `domain_expiry`, 3600 for `tls_cert`, 300 for `flow`, 60 for `heartbeat`, and 10 for `http` / `tcp` / `ping` / `dns`. Enforced on create, bulk, **and** PATCH. |
| `INVITATIONS_LIMIT` | 409 | The org is at its pending-invitation cap. |
| `RATE_LIMITED` | 429 | A per-minute rate budget was exceeded. `Retry-After` (seconds) is set; `details.scope` names the tier, e.g. `per_account_api_writes`. |
| `ABUSE_BLOCKED` | 400 | Target blocked by abuse protection. `details.reason` explains. |
| `URL_PATTERN_BLOCKED` | 400 | Target URL matched an abuse pattern (recon path). |
| `DOMAIN_DENYLISTED` | 400 | Target domain (or a parent) is on the deny-list. |

See [Quotas & rate limits](quotas.md) for the quota model, the per-minute categories, and the deny-list policy.

## Pagination envelope

Every `/api/v1` list endpoint returns:

```jsonc
{ "items": [ /* ... */ ], "limit": 50, "offset": 0, "has_more": true }
```

There is no `total`. `has_more` is `true` when rows exist past this page; the server fetches one extra row to decide it, so listing never pays for a parallel `count(*)`. `limit` defaults to 50 for `/targets`, 100 for `/tags`, 1000 for `/results`, 100 for `/incidents`. `limit` is silently capped server-side: 10,000 for `/targets` and `/results`, 1,000 for `/tags` and `/incidents`.

`GET /api/public/v1/incidents` uses a separate cursor envelope instead, since a parallel `count(*)` over an unbounded public range is too expensive and offset pagination is unstable under inserts:

```jsonc
{ "items": [ /* ... */ ], "next_cursor": "opaque-token-or-null" }
```

Pass the previous page's `next_cursor` as `?cursor=...` to fetch the next page; it's `null` once nothing is left. There is no `limit`/`offset`/`has_more` on this endpoint.

## Results query

`GET /api/v1/targets/{id}/results?from=2026-05-12T00:00:00Z&to=2026-05-12T23:59:59Z&limit=100&offset=0`

- `from` / `to` default to the last 24 h; `to` must be strictly greater than `from` (400 `BAD_TIME_RANGE` otherwise).
- Returns a `PageEnvelope` of `CheckResult` ordered by `timestamp DESC`.
- A failed HTTP result can include a machine-readable `diagnostic` such as `{"kind":"access_interference","confidence":"high","provider":"akamai","evidence":["edge_server","block_page","reference_id"],"remediations":["use_authenticated_health_endpoint","allow_monitor_through_edge_rules"]}`. This explains the failure but never replaces `status`, `response_code`, or `error`, which remain authoritative. The whole field is omitted when no supported signature matches.
- `kind` is one of `access_interference` (a CDN, WAF, or bot-management service blocked the probe by policy, shown by its mitigation header, a challenge, or an access-denied page), `origin_unreachable` (the CDN answered with its own error page because the origin behind it did not respond), or `origin_tunnel_down` (the same, where the edge named a dead tunnel). The two origin kinds carry `provider: "cloudflare"` and a `response_code` of `502`, `520`–`524`, or `530`.
- Bounded provider values are `akamai`, `aws_waf`, `cloudflare`, `azure_front_door`, `data_dome`, and `vercel`; `provider` is omitted when the evidence identifies a generic policy block but not a vendor.
- Bounded evidence values are `challenge_header`, `mitigation_header`, `edge_server`, `block_page`, `reference_id`, and `origin_error_code`.
- Remediation values are stable action codes suitable for API clients: `use_authenticated_health_endpoint`, `allow_monitor_through_edge_rules`, `verify_origin_reachable`, and `verify_edge_tunnel`. They are derived from `kind`, so the same kind always carries the same codes, and they do not imply that hosted probe IPs are static.

## Latency series

`GET /api/v1/targets/{id}/latency?from=…&to=…`

Pre-bucketed quantiles and per-phase means read straight from the per-minute rollup — powers the monitor-detail latency line and phase-breakdown area charts. The server divides the range into ~60 slices (floored to the 60-second rollup grain), so any range returns a comparably dense series and the cost stays O(buckets), not O(samples). Switching range re-scales the buckets.

- `from` / `to` default to the last 24 h; `to` must be strictly greater than `from` (400 `BAD_TIME_RANGE`).

```jsonc
{
  "bucket_seconds": 1440,
  "buckets": [
    {
      "t": 1747137600000,      // unix-ms at bucket start (JS new Date(t))
      "p50": 120, "p95": 180, "p99": 240,
      "avg": 130,              // mean total; breakdown chart derives "processing" = avg − (dns+connect+tls+ttfb)
      "dns": 12, "connect": 20, "tls": 35, "ttfb": 60,  // mean per-phase ms; 0 for kinds that skip the phase
      "samples": 24            // 0 marks a gap the chart leaves unconnected
    }
  ]
}
```

`bucket_seconds` is always a multiple of 60 (1h→60, 24h→1440, 7d→10080, 30d→43200).

### Region filter

`results`, `latency`, `flow-steps`, and `uptime` accept an optional `region=` query parameter to scope the read to one probe region; omit it for an all-regions view. Region ids are the slugs registered via the operator surface. See [Multi-region probes](multi-region.md).

### Per-region latency series

`GET /api/v1/targets/{id}/latency/by-region?from=…&to=…`

Same bucketing and cost as `/latency`, but split by region so each can be overlaid as its own line — powers the monitor-detail overlay chart. One entry per region that has samples in the range; each region's `buckets` use the same shape as `/latency`.

```jsonc
{
  "bucket_seconds": 1440,
  "regions": [
    { "region": "default",  "buckets": [ /* LatencyBucket… */ ] },
    { "region": "eu-west",  "buckets": [ /* LatencyBucket… */ ] }
  ]
}
```

### Per-step duration series

`GET /api/v1/targets/{id}/flow-steps?from=…&to=…&region=…`

One series per declared step of a browser flow: the mean duration of that step among the runs that **passed** it, plus how many failed. A step a run never reached contributes nothing, so a journey that stopped early does not average zeros into the steps behind it.

Failures are counted but kept out of the mean. A failed step sat in its whole `step_timeout` before giving up, so a handful of them bury the timings of every run around them — on a real 14-day window, six failures out of forty-eight runs moved a step's reading from 876 ms to 2017 ms. `avg` is `null` for a bucket no run passed, which is a gap in the timing rather than an instant step; a bucket still appears whenever the step was reached, so a step that only ever fails is distinguishable from one the journey never got to.

Bucketing works like `/latency` but aims for about half as many slices. These render as sparklines a few hundred pixels wide, and at the latency grain a 30-step flow is a 72 KiB response for detail narrower than the line drawing it. `bucket_seconds` tells you what the server picked.

`op` is what the newest run in the range recorded at that index, so a flow edited mid-window is labelled with what it runs today. `step` is the zero-based index into the declared steps.

```jsonc
{
  "bucket_seconds": 1440,
  "steps": [
    {
      "step": 3,
      "op": "assert_url",
      "buckets": [
        { "t": 1747137600000, "avg": 1840, "samples": 5, "failed": 0 },
        { "t": 1747181000000, "avg": null, "samples": 0, "failed": 3 }  // reached, never passed
      ]
    }
  ]
}
```

## Uptime query

`GET /api/v1/targets/{id}/uptime?from=…&to=…`

```jsonc
{ "total": 8640, "up": 8635, "down": 0, "degraded": 0, "error": 5, "uptime_pct": 99.94 }
```

## Incidents query

`GET /api/v1/targets/{id}/incidents?from=…&to=…&ongoing_only=false&limit=100&offset=0`

Returns coalesced down / error periods. A contiguous run of bad statuses becomes one incident; an `up` result between two bad runs splits them. Ongoing incidents return `ended_at: null` and `duration_secs: null`.

```jsonc
{
  "items": [
    {
      "id": "01h7m8z4n6v0e1m7v7y6x8x8x8",
      "target_id": "01h7m...",
      "started_at": "2026-05-13T11:30:00.000Z",
      "ended_at":   "2026-05-13T11:35:00.000Z",
      "status":     "down",
      "duration_secs": 300,
      "check_count": 5,
      "error_sample": "connection refused"
    }
  ],
  "limit": 100, "offset": 0, "has_more": false
}
```

## Tags inventory

`GET /api/v1/tags?q=prod&limit=100`

Returns every tag currently in use across the caller's targets (enabled or disabled), with target count, sorted by descending count then alphabetical. `q` is a prefix filter for autocomplete. Scoped to the active org — in SaaS mode another org's tags are invisible.

```jsonc
{ "items": [ { "name": "prod", "count": 12 }, { "name": "staging", "count": 4 } ],
  "limit": 100, "offset": 0, "has_more": false }
```

## Dashboard summary

`GET /api/v1/dashboard/summary` — per-org rollup cached in-process for 5 seconds (keyed by `OrgId`, so two tenants never share an entry).

```jsonc
{
  "targets":        { "total": 42, "enabled": 40, "disabled": 2 },
  "current_status": { "up": 38, "down": 1, "degraded": 1, "error": 0, "unknown": 2 },
  "last_24h":       { "checks_total": 50400, "checks_up": 50360, "uptime_pct": 99.92, "incidents": 3 },
  "system":         { "in_flight_checks": 5, "result_queue_depth": 12, "dropped_results_last_5m": 0, "circuit_breakers_open": 0 }
}
```

`current_status` counts each monitor once, so its five buckets sum to `targets.total`. A monitor with no readable result in the last 24 hours counts as `unknown`. The state is folded across probe regions under the monitor's region policy, matching the console and the MCP tools: failing regions below the monitor's quorum count as `degraded`, not `down`. See [Multi-region probes](multi-region.md#incident-detection-across-regions).

## On-demand operations

- **`POST /api/v1/targets/test`** — runs one check against a raw `CheckSpec`, no persistence. Same SSRF / URL-scheme / port validation as `POST /targets`. Returns `TestResponse { result, matched_expectations, warnings }`.
- **`POST /api/v1/targets/{id}/check-now`** — runs one check against an existing target using its stored credentials, dispatched to an agent in the target's region. Result is persisted. Returns `503 PROBE_UNAVAILABLE` if no agent is currently serving the region.
- **`POST /api/v1/targets/bulk-action`** — apply one action atomically to up to 10,000 ids. Partial failure allowed; the response lists `succeeded` and `failed` separately, with per-id `code` + `message`.

```jsonc
{
  "ids": ["01h7m...", "01h7n..."],
  "action": { "type": "disable" }
  // alternatives: { "type": "enable" }, { "type": "delete" },
  //   { "type": "tag_add",    "tags": ["frozen"] },
  //   { "type": "tag_remove", "tags": ["frozen"] },
  //   { "type": "set_group",  "group": "tier1" } // null clears the group
}
```

`tag_add` merges into what each monitor already carries, so a target whose merged list would go over the 50-tag limit is left untouched and reported in `failed` as `TOO_MANY_TAGS`. `tag_remove` is not held to the tag rules, so a tag that predates them can still be cleaned up.

## Idempotency

`POST /api/v1/targets/bulk` and `POST /api/v1/targets/bulk-action` accept an optional `Idempotency-Key` header. The server stores the response for 24 hours keyed by `(header value, body hash)`. A retry with the same key and body returns the original response without re-executing. A retry with the same key but a different body executes normally — the body hash is part of the cache key. The cache is in-process; entries are lost on restart.

```http
POST /api/v1/targets/bulk-action HTTP/1.1
Idempotency-Key: 01h7m8z4n6v0e1m7v7y6x8x8x8
Content-Type: application/json

{ "ids": ["..."], "action": { "type": "disable" } }
```
