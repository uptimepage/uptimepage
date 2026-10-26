# REST API

Mounted under `/api/v1` on the configured API bind. JSON in, JSON out. Every `/api/v1/*` endpoint is authenticated: a session cookie from the operator UI, or an `Authorization: Bearer` API token bound to one org and carrying its own scopes. The health probes, the OpenAPI document, and `POST /api/v1/invitations/decline` (token in body, not a session or API token) are the exceptions and stay open. See [Authentication](authentication.md#api-token-auth).

OpenAPI 3.1 document at `GET /api/openapi.json`; Swagger UI at `GET /docs` on the app host (<https://app.uptimepage.dev/docs> on the hosted service).

All responses use `Content-Type: application/json; charset=utf-8`.

### Discovery

Automated clients that only know the website can find the API without being told its paths. On the hosted service, `https://uptimepage.dev` publishes an [RFC 9727](https://www.rfc-editor.org/rfc/rfc9727) catalog at `GET /.well-known/api-catalog` (`application/linkset+json`) listing the REST API and the MCP server, each with links to its OpenAPI document and its documentation page. Every page there also carries an [RFC 8288](https://www.rfc-editor.org/rfc/rfc8288) `Link` header with `rel="api-catalog"`, `rel="service-desc"` and `rel="service-doc"`, so one `HEAD /` is enough to bootstrap. With `site.origin` set, the app host redirects `/.well-known/api-catalog` to the site's catalog.

When the MCP server is enabled it publishes a Server Card at `GET /.well-known/mcp/server-card.json` on the MCP host, built from the same server info the protocol's `initialize` returns. The website redirects the same path there, so an agent that only knows the domain still finds it.

On the website, documentation pages, blog posts and the homepage also answer `Accept: text/markdown` with their Markdown source instead of HTML, at `text/markdown; charset=utf-8` with a `x-markdown-tokens` size hint. Responses carry `Vary: Accept` and a per-representation `ETag`, so browsers keep the rendered page.

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
| `POST` | `/api/v1/targets/{id}/duplicate` | create up to 100 targets that copy an existing one, each with its own name and address |
| `POST` | `/api/v1/targets/bulk-action` | enable / disable / delete / tag-add / tag-remove / set-group / set-interval / channel bindings on many ids |
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
| `GET` | `/api/v1/targets/{id}/state` | a manual monitor's state |
| `PUT` | `/api/v1/targets/{id}/state` | set a manual monitor's state |
| `GET` | `/api/v1/regions` | list the enabled probe-region catalog: `{ "regions": [...] }`, each entry `id`, `name`, `city`, `country_code`, `continent`, `latitude`, `longitude` |
| `GET` | `/api/v1/targets/{id}/incidents` | coalesced incident periods (`from`, `to`, `ongoing_only`) — paginated |
| `POST` | `/api/v1/targets/{id}/shares` | mint a read-only share link; returns the share (token included) |
| `GET` | `/api/v1/targets/{id}/shares` | list a monitor's live share links (token included, re-copyable) |
| `DELETE` | `/api/v1/targets/{id}/shares/{share_id}` | revoke a share link |
| `GET` | `/api/v1/account/holds` | list monitors and status pages the plan is holding |
| `PUT` | `/api/v1/account/holds` | choose what the plan keeps (`keep_monitors`, `keep_status_pages`) |
| `GET` | `/api/v1/account/billing` | subscription status, plan, period end, the cadence it bills on (`interval`), a booked move and the cadence it lands on (`pending_plan_id`, `plan_change_at`, `pending_interval`), and the plans on sale with their price (`amount_minor`, `currency`) per cadence (session only) |
| `POST` | `/api/v1/account/billing/checkout` | start a checkout for a plan (`plan_id`, `interval`, `accept_terms: true`); returns a provider URL |
| `POST` | `/api/v1/account/billing/portal` | open the provider's customer portal (invoices, card, cancel) |
| `PUT` | `/api/v1/account/billing/plan` | move the subscription to another plan — up now, down at period end |
| `POST` | `/api/v1/account/billing/cancel` | cancel: at the end of the paid period, or at once while the last payment failed |
| `DELETE` | `/api/v1/account/billing/cancel` | withdraw a scheduled cancel |
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
| `GET` | `/api/v1/maintenance` | list windows (`status=active\|upcoming\|past\|all`, paginated; `upcoming` lists the soonest first) |
| `GET` | `/api/v1/maintenance/{id}` | get one window |
| `PATCH` | `/api/v1/maintenance/{id}` | edit title / description / time range / components / alert suppression (rejected after `ends_at` or once cancelled; `ends_at` must stay in the future, except that on a running window an `ends_at` at or before now ends it at the server's clock) |
| `DELETE` | `/api/v1/maintenance/{id}` | cancel a window that has not ended (kept as history with `deleted_at` / `deleted_by`) |
| `POST` | `/api/v1/maintenance/{id}/updates` | post a status update to an upcoming, running or completed window; it shows on the status page and goes to subscribers (`422 MAINTENANCE_CANCELLED` on a cancelled window, `422 MAINTENANCE_UPDATE_LIMIT` past 50) |
| `PATCH` | `/api/v1/incidents/{id}` | update narration: `title`, `public_title`, `public_description`, `severity` (JSON `null` clears, omit to leave alone; a declared incident's `title` cannot be cleared), plus `counts_as_downtime` on a declared incident (`422` on a monitor-opened one) |
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
| `GET` | `/api/v1/status-pages/{id}` | one page + its live URL, logo URL and `access` |
| `PATCH` | `/api/v1/status-pages/{id}` | rename, change slug, publish/unpublish, set the language (`public_locale`), edit branding, change who may open it (`access`) |
| `DELETE` | `/api/v1/status-pages/{id}` | delete the page |
| `GET` | `/api/v1/status-pages/{id}/components` | the monitors curated onto the page |
| `POST` | `/api/v1/status-pages/{id}/components` | add a monitor (distinct-target cap `max_public_components`) |
| `PATCH` | `/api/v1/status-pages/{id}/components/{target_id}` | per-page `public_name` / `public_description` / `public_group` (JSON `null` clears) |
| `DELETE` | `/api/v1/status-pages/{id}/components/{target_id}` | remove a monitor from the page |
| `POST` | `/api/v1/status-pages/{id}/components/reorder` | set component order |
| `POST` | `/api/v1/status-pages/{id}/logo` | upload a logo (multipart) |
| `DELETE` | `/api/v1/status-pages/{id}/logo` | remove the logo |
| `GET` | `/api/v1/status-pages/{id}/access/viewers` | who signed in to a private page |
| `POST` | `/api/v1/status-pages/{id}/access/viewers/{viewer_id}/block` | sign a viewer out, remove their passkeys and subscription, and keep them out |
| `POST` | `/api/v1/status-pages/{id}/access/viewers/{viewer_id}/unblock` | lift a block |
| `GET` | `/api/v1/status-pages/{id}/access/viewers/{viewer_id}/history` | a viewer's visit days and sign-in events, kept a year |
| `GET` | `/api/v1/status-pages/{id}/access/password` | the page password's use: `enabled`, `set_at`, `set_by`, devices signed in (`sessions`), `last_sign_in_at`, `failed_last_24h`, and the `subscriptions` a change would end |
| `POST` | `/api/v1/status-pages/{id}/access/password/sign-out` | sign out every device signed in with the password; the password still works |

`access` is who may open the page. It is `null` on a page never made private,
and as a write it replaces what is stored, whole. `POST` and `PATCH` take it
beside the other fields and commit it in the same transaction:

```json
{
  "access": {
    "protected": true,
    "session_days": null,
    "email": { "enabled": true, "passkeys_enabled": true, "recheck_days": null },
    "password": { "enabled": true, "value": "…" },
    "rules": [
      { "kind": "domain", "value": "example.com" },
      { "kind": "address", "value": "name@partner.example" }
    ]
  }
}
```

`protected: true` needs a plan with private pages (`403 PRIVATE_PAGES_DISABLED`
otherwise; a page that already is private stays editable after its plan loses
them) and a way in: the email code on with at least one rule, the password on,
or both. `session_days` (7, 30, 90 or `null`) applies to email and password
sign-ins alike; `recheck_days` is 30, 90, 365 or `null`. Entries are normalised
(lower case, an international domain in its ASCII form) and deduplicated, up to
500. Anything else is `400 ACCESS_INVALID`, with `field` naming the part.

`password.value` is write only: it sets or replaces the page password, kept as
sent, 1 to 128 characters (`400 INVALID_PAGE_PASSWORD` otherwise). Reads return
`password: { "enabled": … }` and never the value, so a GET then a PATCH of what
it said keeps the stored password. Without `value` the stored one stays, on or
off; `enabled: true` with none stored is `400 PASSWORD_REQUIRED` (field
`access.password.value`). `value` with `enabled: false` stores it and leaves the
way in off. Audit entries say a password was `set` or `changed`, never what it
is. See [Who can open a page](public-status.md#who-can-open-a-page).

### Public status endpoints

Unauthenticated; mounted at `/api/public/v1/*` and bypassed at Caddy via the
`@public` matcher (see [Deployment](deployment.md#public-status-surface)).
Each response carries `Cache-Control: public, max-age=10,
stale-while-revalidate=30`. A [private page](public-status.md#who-can-open-a-page)
answers these only to a signed-in viewer, with `Cache-Control: private,
no-store`, and anyone else with `401 PAGE_SIGN_IN_REQUIRED`. Its feed also
answers a viewer's own link (`?viewer=…&t=…`) or the page-password link
(`?t=…`, which stops when the password changes), for readers that carry no
cookie. The page password itself is never accepted in a URL or a header. A monitor not curated onto the page being
served is invisible on every public surface — direct lookups return 404
and it never appears in any list. Wire types literally cannot serialise
sensitive target fields (`url`, `headers`, `basic_auth`, `bearer_token`).

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/` | server-rendered HTML status page on a page's own host (`?fragment=1` returns the dynamic region only) |
| `GET` | `/status` | the same page on a single-tenant deploy, where `/` is the operator dashboard; on a page's own host it redirects to `/` |
| `GET` | `/status/incidents/{id}` | per-incident detail page |
| `GET` | `/api/public/v1/status` | the same data as `/status` in JSON; each component carries `uptime_pct` (time-based, `null` until probed) and each incident an `impact` of `degraded` \| `partial_outage` \| `major_outage` beside its declared `severity`; `recent_maintenance` lists the finished windows that started in the last 30 days |
| `GET` | `/api/public/v1/components/{id}/history` | per-component history (`days` query, 1..365, default 90) |
| `GET` | `/api/public/v1/incidents` | recent public incidents (paginated) |
| `GET` | `/api/public/v1/incidents/{id}` | one public incident with its update timeline |
| `GET` | `/api/public/v1/incidents.rss` | RSS 2.0 feed of recent incidents |
| `GET` | `/api/public/v1/maintenance` | active + upcoming maintenance windows (the next 30 days), each with its `updates` |
| `GET` | `/api/public/v1/maintenance/past` | maintenance windows that have ended, newest first, each with its `updates` (those that ran in the last 90 days, paginated like the incident list) |
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

### Subscriptions

Present only when the hosted service has a payment provider configured; a self-host build answers `404 BILLING_UNAVAILABLE`. The endpoints under `/api/v1/account/billing` are session-only, like managing API tokens — a scoped API token can never buy, move or cancel a plan — and act on the account behind the caller's active org, which the caller must own; an org owner who is not the account's payer gets `403 ACCOUNT_OWNER_REQUIRED`, the same gate as the holds.

An upgrade applies at once and is prorated by the provider; a downgrade or a cancel takes effect at the end of the paid period, and until then the account keeps what it paid for. Only a strictly bigger plan is charged on the spot: returning to the plan already paid for while a downgrade is booked, or changing cadence (`interval` of `month` or `year`), switches the price without a charge and takes effect at the renewal. Deleting the account (`DELETE /api/v1/me`) ends a paid subscription at the provider first, and is refused if the provider will not end it. Nothing is ever deleted: if a smaller plan does not cover everything, the excess is held (see [Quotas](quotas.md#when-a-plan-no-longer-covers-what-an-account-has)) and the owner picks what to keep. The provider's webhook is the authority — the API returns the account's state as it stands after the provider replied, but a change is confirmed by the event that follows, not the button that asked for it. `PLAN_NOT_FOR_SALE` means the plan is not sold on that cadence; `SUBSCRIPTION_STATE` means the action does not apply to where the subscription stands (buying when one is already live, cancelling when none is, changing to the plan and cadence already in force or already booked, changing the plan while a cancel is booked).

## Check specs

Tagged enum, `type` discriminator.

### IP version

`http`, `tcp` and `tls_cert` checks take `ip_version`: `any` (the default), `ipv4` or `ipv6`. With `any` the probe races IPv6 against IPv4 and keeps whichever connects first, so a broken AAAA record or an IPv6 firewall stays invisible while IPv4 works. `ipv4` asks DNS for A records only and `ipv6` for AAAA records only, and the check dials nothing else; an HTTP check keeps the family on every redirect hop. A name with no address in the chosen family fails the check with `dns: no IPv4 address` or `dns: no IPv6 address` rather than falling back to the other one. To watch both families, create one monitor for each. A host written as an IP address already decides the family, so pinning it to the other one is `400 INVALID_IP_VERSION`. Ping, DNS, domain expiry and flow checks take no `ip_version`.

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
  "bearer_token": null,
  "ip_version": "any"                           // any, ipv4 or ipv6
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
{ "type": "tcp", "host": "db.internal", "port": 5432, "timeout": 2000, "ip_version": "any" }
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
  "expected_contains": "192.0.2.1", // optional substring match
  "timeout": 3000
}
```

```jsonc
{
  "type": "dns",
  "domain": "example.com",
  "record_type": "MX",
  "expected_records": ["10 mail.example.com.", "20 alt.example.com."], // optional exact set
  "timeout": 3000
}
```

Resolves `domain` and reads the answers. `record_type` is one of `A`, `AAAA`,
`CNAME`, `MX`, `NS`, `TXT`, `SOA`, `PTR`, `CAA`, `SRV`. Surrounding whitespace
and a trailing dot on the domain are tolerated, and so are underscore labels, so `_dmarc.example.com`,
`s1._domainkey.example.com` and `_sip._tcp.example.com` can be watched;
host-based checks still refuse them. A leftmost `*` label queries the wildcard
record itself, the one that answers for names that do not exist in the zone;
`*` anywhere else is refused. A `PTR` check takes either the address
(`192.0.2.1`, `2001:db8::1`), which is looked up as its `in-addr.arpa` or
`ip6.arpa` name, or that reverse name itself; every other record type refuses
an address with `400 INVALID_DNS_PARAMS`. `resolver` takes `ip` or `ip:port`
(`1.1.1.1`, `8.8.8.8:53`) to query one specific server instead of the process
default.

Status: an empty answer set — including NXDOMAIN and no-records — is `down`.
With `expected_contains` set, an answer set where no value contains that
substring is also `down`, which is how a hijacked or mis-pointed record
surfaces rather than passing because *something* resolved. Without it, any
non-empty answer is `up`. A resolver failure (SERVFAIL, network error) is also
`down`; only the outer check timeout records an `error`.

`expected_records` is the strict form: list every record the name should
hold, and the check is `down` when the answer differs in any way, a record
added, removed or changed. Write each value as the result's `answers` shows
it: `192.0.2.1` for A, `10 mail.example.com.` for MX, `0 issue
"letsencrypt.org"` for CAA, the joined text for TXT. Zone-file spellings work
too: quoted TXT strings (`"v=DKIM1; " "p=..."`), `xn--` labels and leading
zeros. Order and TTL are ignored. Names compare case-insensitively with the
trailing dot optional, addresses by value (`2001:0db8::1` is `2001:db8::1`),
and TXT exactly, case included. An SOA value carries the zone's serial, so an
SOA set fails on every zone edit. An MX value may also be a bare host
(`smtp.google.com`): it stands for one record for that host at any
preference, so a preference change passes while a record added, removed or
replaced still fails; write the preference to watch it too. When the host
answers more than once, the bare value takes the lowest preference and the
rest are unexpected. Up to 50 values of up to 2048 characters each; an empty list is the
same as none. Setting both `expected_records` and `expected_contains`, a value
not in the type's form (an A value that is not an IPv4 address, a bare number
or address as an MX host), or one value twice is `400 INVALID_DNS_PARAMS`.

Every result that got an answer carries `details` with `domain`,
`record_type`, the `answers` list (records of the queried type),
`expected_contains`, and `matched`, so the current records are readable while
the check is up; a failure repeats the same document as its `error`. On an aliased name the CNAME chain that led to the answers
is listed apart as `aliases`; `expected_contains` matches it too, an exact set
does not.
With `expected_records` set it also carries `missing` (expected values no
answer carries) and `unexpected` (answers nothing expected).

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

Constraints: `period` runs from 60 s to 30 d, `grace` from 0 to 30 d, `max_runtime` from 60 s to 30 d, and the monitor's `interval` is the evaluation cadence: it has a 60 s kind floor and may not be coarser than a tenth of `period + grace` (held between 60 s and 300 s, or the plan floor if that is higher), since a coarser tick only delays the alarm; a PATCH that shrinks the window through `check` alone lowers a coarser stored interval to the new cadence. A ping that changes the verdict, a success that ends a silence or a `fail` after successes, writes its result the moment it arrives rather than at the next tick; routine pings write nothing. Heartbeats never run on regional probes and reject `test`/`check-now` (`HEARTBEAT_NOT_PROBEABLE`) and region assignment, since there is nothing to probe. Unknown ping tokens 404 uniformly; per-token ping ingest is rate-limited (429 with `Retry-After`) at 1/s sustained with a burst of 10, which is shared across all of a token's signals.

Provisioning: a single create mints the ping URL immediately; bulk-created heartbeat monitors get theirs within one scheduler refresh (~30 s). Pick `grace` with your deployment in mind: a ping sent while the control plane itself is unreachable is lost, so on single-node self-hosts keep `grace` comfortably above your restart window (the hosted service deploys blue/green, so ping ingest stays up).

### Manual

```jsonc
{ "type": "manual" }
```

No probe and no fields: the state is whatever an operator last set. A new manual monitor is `up`. `GET /api/v1/targets/{id}/state` (`targets:read`) returns `{ "status": "down", "note": "carrier reports a trunk outage", "set_at": "...", "set_by": "<user id>" }`, where, until the first set, `set_at` is the creation time and `set_by` is null; `set_by` is null again once the setter's deleted account is purged. `PUT` on the same path (`targets:write`) takes `{ "status": "up" | "degraded" | "down", "note": "..." }` and returns the same shape. `note` is optional, one line of at most 200 characters, and replaces the previous one; an invalid note is `400 INVALID_MANUAL_STATE`. Either route on another kind of monitor is `404 MANUAL_NOT_CONFIGURED`.

A set that changes something writes its result at once, so `down` or `degraded` opens an incident on the incident writer's next pass (about 30 s) and `up` closes it; the scheduler restates the state once a minute after that. A manual monitor needs one bad result, not `alert_confirmations` of them, so create stores `alert_confirmations: 1` whatever the body says and a PATCH to another count is `400 INVALID_ALERT_CONFIG`; its `interval` is 60 s, and a create or PATCH naming any other is `400 INVALID_INTERVAL`. While an incident is open, a worse state raises it (degraded to down) instead of opening another, so status pages follow, and a system note on its internal timeline records when. Each down or degraded set replaces the incident's cause with its note. Every change is audited as `target.state_set` with the acting user; a set that matches the current state and note changes nothing and is not audited. A paused monitor, or one your plan no longer covers, keeps the state you set and reports it once it runs again. Manual monitors reject `test`/`check-now` (`HEARTBEAT_NOT_PROBEABLE`) and region assignment.

### TLS certificate expiry

```jsonc
{
  "type": "tls_cert",
  "host": "example.com",
  "port": 443,
  "server_name": null,         // optional SNI override; defaults to `host`
  "warn_days": 14,
  "critical_days": 7,
  "timeout": 10000,
  "ip_version": "any"          // any, ipv4 or ipv6
}
```

Opens a TCP connection, performs a TLS handshake against the host (accepting any presented chain so that expired or self-signed certs can still be inspected), and parses the leaf certificate's `notAfter`. Status mapping:

- `days_remaining < 0` (expired) → `down`
- `days_remaining < critical_days` → `down`
- `days_remaining < warn_days` → `degraded`
- otherwise → `up`

Every result that read a certificate carries `details` with `days_remaining`, `not_after`, `subject_common_name`, `issuer_common_name`; a `degraded` or `down` result repeats that document as its `error`. A handshake failure (plain-TCP host, network error) returns `error` status with the underlying message. `warn_days` must be strictly greater than `critical_days`. Floor is `interval >= 3600` (enforced); a monitor created in the form opens at `43200` (twice daily), which leaves room for a short-lived certificate whose `warn_days` is only a day or two.

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

Queries the [IANA RDAP bootstrap registry](https://data.iana.org/rdap/dns.json) to find the authoritative RDAP server for the domain's TLD, then fetches `/domain/<domain>` and reads the `events[?eventAction == "expiration"]` entry. Status mapping is the same as TLS cert: `< critical_days` → `down`, `< warn_days` → `degraded`, else `up`. Every result that knows the expiry carries `details` with `domain`, `days_remaining`, `expiration_date`, and `registrar` (`null` when the registry names none); non-`up` results repeat that document as their `error`.

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
  "owner_user_id": "…",      // optional member of the org. Left out on create,
                             // the caller owns it, so name one when the token
                             // is a shared pipeline's; null leaves it unowned,
                             // and clears it on PATCH
  "alerts": [ /* optional, see below */ ],
  "alert_confirmations": 2,   // see "Alert config" below, with
  "notify_recovery": true     // renotify_interval_secs and region_policy
}
```

Server returns the full `Target` including `id` (UUIDv7), `created_at`, `updated_at`, and `write_source`. The assigned regions are read back from `GET /api/v1/targets/{id}/regions` and changed with `PUT` on the same path; a `regions` array that names an unknown or disabled id, or any id on a heartbeat or manual monitor, is `422 REGION_INVALID`, and one wider than the plan allows is `422 QUOTA_EXCEEDED`. A `POST /api/v1/targets/bulk` item takes the same field; an item without it shares the default set.

A monitor's kind is fixed after creation. A `check` sent on `PATCH` must keep the stored `type`; one whose `type` differs is `400 Bad Request` (`CHECK_KIND_IMMUTABLE`, `field: check.type`), since the monitor keeps its id and would otherwise carry two kinds' results in one history. Create a new monitor to watch something else.

Every JSON request body, on `/api/v1` and on the operator and sign-in endpoints alike, may carry only the keys its resource documents. Anything else, a misspelt key or a read-only field echoed back from a `GET` such as `id` or `write_source`, is `422 INVALID_JSON` naming the key, so a setting under the wrong name fails loudly rather than silently leaving the default in place. A body that is not JSON at all is `400 INVALID_JSON`; one sent without `Content-Type: application/json` is `415 INVALID_CONTENT_TYPE`. A key inside a nested object is refused the same way and named by its path, `check.timeuot` or `check.steps[1].selctor`; the message lists what that object accepts. Map keys, such as the names under `check.headers`, are free.

`write_source` is a read-only field recording where the resource was last
written from: `ui`, `api`, or `terraform` (decided server-side from the
request, never the body). It also appears on
notification channels and maintenance windows, and drives the "managed by"
badge in the web UI. A write through any endpoint restamps it, so it reflects
the most recent author.

### Duplicating a target

`POST /api/v1/targets/{id}/duplicate` creates new targets that start as an exact copy of the stored one. Each copy names itself and, optionally, what it checks:

```jsonc
{
  "copies": [
    { "name": "api eu", "address": "https://eu.example.com/health" },
    { "name": "api us", "address": "https://us.example.com/health" },
    { "name": "api again" } // no address: same as the source
  ]
}
```

The `address` is a URL for `http` and `flow` (the flow's `start_url`; its steps are copied as they are), a host or `host:port` for `tcp` and `tls_cert` (a bare host keeps the port, and a `tls_cert` copy keeps any `server_name` override), a host for `ping`, and a domain for `dns` and `domain_expiry`. Heartbeat and manual targets have none, and naming one is `400 INVALID_ADDRESS`.

Everything else is copied from the stored row, so nothing has to be echoed back: the check with its credentials (`basic_auth`, `bearer_token`, flow `fill` values), the interval, the paused or running state, tags, group, alert bindings, and the regions and `region_policy`. The interval and region set are copied as stored even when the plan now allows less; the plan governs them at probe time, as it does for the source. The escalation policy is copied only when the caller could bind it by hand, since a new monitor on a ladder is new paging coverage: the org owner, on a plan with on-call, with a token that allows `oncall:write`. Otherwise the copies have no policy of their own and follow the org default, if one is set, like any new monitor. A flow copy is held to the plan's flow limits like any new flow, so a source with more steps than the plan now allows is `400 INVALID_FLOW_PARAMS`, and one whose regions no longer have a flow-capable agent is `422 NO_FLOW_CAPABLE_AGENT`. The caller owns every copy. A heartbeat copy gets its own ping URL at once. Status page components, share links and history are not copied.

The response is `201` with the new targets in request order, redacted as usual. The whole batch is created or none of it: a copy that fails validation is `400` with `field` naming it (`copies[1].address`, `copies[0].check.url`), and a batch the plan has no room for is `422 QUOTA_EXCEEDED`. More than 100 copies is `413 BULK_TOO_LARGE`; for more, use `POST /api/v1/targets/bulk`. The endpoint accepts an `Idempotency-Key` header.

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
"recovery_period_secs": 600,
"region_policy": "majority"
```

- `channel_id` — id of a notification channel owned by the **same org**. A binding to an unknown or another tenant's channel is rejected. It is the binding's only key; the firing policy is the monitor's `alert_confirmations` and `notify_recovery` below, and a per-binding key is `422`.
- `alert_confirmations` — consecutive failing checks before an incident opens (and the same number of passing checks before it closes, which damps flapping). Default `2`, must be `>= 1`.
- `notify_recovery` — when `true` (default), the recovery is announced to the monitor's channels. When `false`, recovery is silent.
- `renotify_interval_secs` — seconds before the first reminder while an outage stays unacknowledged. Each further reminder doubles the gap, capped at a day, so a long outage nobody answers decays to a daily nudge. An interval already longer than a day keeps its own cadence. `0` disables reminders; otherwise must be `>= 60`. Default `3600`. Acknowledging or resolving the incident stops the reminders.
- `recovery_period_secs` — how long the monitor must stay up before its incident closes. A failure that confirms inside the window keeps the same incident open and restarts the wait, so a service that flaps for an hour is one incident rather than a dozen short ones. The incident's end is still dated to when the final recovery began, so the wait is not counted as downtime. While it waits the incident carries `recovering_since`: paging pauses and the status page shows the component up, so only the recovery notice comes that much later. A failure that confirms inside the wait ends that recovery, and the stretch it lasted is listed in the incident's `recovered` as `{from, until}`; uptime leaves it out. `0` closes as soon as the passing checks confirm; at most `1800`. Omitted on create, it is `180` on an `http`, `tcp`, `ping`, `dns` or `flow` monitor and `0` on the rest: a heartbeat waits on the job's next ping and an expiry check runs hours apart, so a hold would keep a recovered incident open until then. A manual monitor closes on the state it is set to, so anything but `0` there is `400 INVALID_ALERT_CONFIG`.
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
- `recovery_period_secs must be between 0 and 1800`

A `region_policy` of `{ "count": N }` where `N` is `0` or exceeds the
available regions is `422 INVALID_REGION_POLICY`. A count within the catalog but
wider than the regions one monitor is assigned is accepted and clamped to the
regions that report, since a monitor's region set can be narrowed at any time
after it is created.

### Validation errors

`POST` and `PATCH` return `400 Bad Request` for:

- Unsupported URL scheme (`url scheme '...' not allowed` — only `http` and `https`)
- Missing URL host, empty TCP host, or TCP/TLS port `0`
- `tls_cert warn_days must be > critical_days` (each must also be in `1..=365`)
- `domain_expiry domain must be of the form 'name.tld'` (no dot, or an empty label on either side of it)
- `domain_expiry warn_days must be > critical_days` (each must also be in `1..=365`)
- `domain_expiry` create is refused when the domain's TLD registry publishes no expiry data: the check could never succeed
- **SSRF guard** — `target address ... is in a blocked range`. Triggered when the URL or TCP host is an IP literal that resolves to loopback / private / link-local / reserved space (see [Configuration → `security.allow_private_targets`](configuration.md)). Hostname literals are checked again at connect time after DNS resolution, so DNS rebinding cannot bypass the guard.
- **Mail port** (`MAIL_PORT_NOT_ALLOWED`): `port 25 is a mail port; watch a mail server with a TCP or TLS certificate monitor`. An HTTP URL, a flow's start URL or `goto` step, and a DNS check's custom resolver may not use ports 25, 465, 587 or 2525. TCP and TLS certificate monitors may. A redirect to one of these ports fails the check at connect time with `mail port not allowed`.
- **Redaction sentinel** — `basic_auth contains redaction sentinel — re-supply the real credential` or the equivalent for `bearer_token`. Rejected to prevent a `GET` → `PATCH` round-trip from silently overwriting the stored credential with `"***"`.
- **Kind change** (`PATCH` only) — `a monitor's check type is fixed after creation; create a new monitor to watch something else` (`CHECK_KIND_IMMUTABLE`). The stored `check.type` is the only one a `PATCH` may send.
- **IP version** — `2001:db8::1 is an IPv6 address; set ip_version to any or ipv6` (`INVALID_IP_VERSION`). A host written as an IP address can only be reached over its own family.
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
  "auto_bind_tags": ["db"],
  "acknowledge_button": true,
  "resolve_button": false
}
```

`auto_bind_tags` is the channel's tag rule: on top of the monitors bound to it, the channel pages any monitor carrying at least one of these tags, resolved when the alert fires. Optional on create, replaced whole on `PATCH`, and `[]` clears it. Tags obey the same rules as monitor tags, except that matching ignores case: a rule reading `DB` covers a monitor tagged `db`, and two spellings of one tag are stored once. Tag *filters* elsewhere in the API stay exact.

`acknowledge_button` says whether alerts for an open incident carry an Acknowledge button. Optional, `true` by default. Only the kinds that can carry one read it: `slack`, `slack_app`, `discord`, `discord_app`, `msteams`, `google_chat`, `mattermost`, `email`, `ntfy` and `telegram_app`.

`resolve_button` says whether those alerts also carry a Resolve button, which closes the incident from the alert for a signed-in member or a chat account a member linked. Optional, `false` by default, and read by the same kinds except `ntfy`, whose link cannot say who pressed it.

`config` is `type`-tagged. Supported transports:

- `slack` — `{ "type": "slack", "webhook_url": "https://…", "mention": "@here S01ABC234" }` (incoming webhook; posts `{ "text": "…" }`. `mention` is optional: `@here`, `@channel`, user-group ids (`S…`) or member ids (`U…` / `W…`), space or comma separated, up to 5. It leads the text on opened/reopened/escalated/no-data only, is dropped for `@here`/`@channel` on test sends, and is not a secret, so it is returned unmasked)
- `slack_app` — `{ "type": "slack_app", "webhook_url": "https://…", "channel": "#ops", "channel_id": "C…", "team_id": "T…", "mention": "@here S01ABC234" }` — connected with **Add to Slack**, so a press on its alerts' Acknowledge button reaches the platform's Slack app. **Not creatable from request bodies** (`422 CHANNEL_KIND_MANAGED`): a caller-supplied channel id would point presses at a channel the webhook does not post to. Created only by the Add to Slack flow; `webhook_url` is returned masked. A `PATCH` may send `{ "type": "slack_app", "mention": "…" }` to change the ping, which follows the `slack` rules. The webhook, channel and workspace stay the connected ones: they may be left out or sent back as read, and a body naming others is refused with `422 CHANNEL_KIND_MANAGED`; connect the new channel with Add to Slack to move it. `POST …/{id}/test` takes `{ "config": { "type": "slack_app", "mention": "…" } }` to try a ping before saving it
- `discord` — `{ "type": "discord", "webhook_url": "https://discord.com/api/webhooks/…", "mention": "@here &123456789012345678" }` (channel webhook; posts an embed with `?wait=true` so delivery failures surface synchronously. `mention` is optional: `@everyone`, `@here`, role ids (`&123…`) or member ids (`123…`), space or comma separated, up to 5. A role id and a member id share one shape, so a role must carry the leading `&` or it resolves to nobody. Discord resolves no mention inside an embed, so it rides the message content, and `allowed_mentions` names exactly those targets and nothing else. Same lifecycle as Slack: opened/reopened/escalated/no-data only, `@everyone`/`@here` dropped on test sends, returned unmasked because it is not a secret)
- `discord_app` — `{ "type": "discord_app", "webhook_url": "https://discord.com/api/webhooks/…", "webhook_id": "123…", "mention": "@here &123456789012345678" }` — connected with **Add to Discord**, so the webhook belongs to the platform's Discord app and a press on its alerts' Acknowledge button reaches that app. **Not creatable from request bodies** (`422 CHANNEL_KIND_MANAGED`): a caller-supplied webhook id would point presses at a webhook the alerts do not go through. Created only by the Add to Discord flow; `webhook_url` is returned masked. A `PATCH` may send `{ "type": "discord_app", "mention": "…" }` to change the ping, which follows the `discord` rules. The webhook stays the connected one: it may be left out or sent back as read, and a body naming another is refused with `422 CHANNEL_KIND_MANAGED`. `POST …/{id}/test` takes `{ "config": { "type": "discord_app", "mention": "…" } }` to try a ping before saving it
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
- **Test sends** deliver one clearly-labelled synthetic alert. The per-channel form tests the stored config (works on a disabled channel too). For a `slack_app` or `discord_app` channel its body may carry an unsaved ping as `{ "config": { "type": "slack_app", "mention": "…" } }`, sent on the stored connection and nothing saved; that needs `channels:write` besides `channels:execute`, and any other body is refused. The collection-level `POST …/test` takes `{ "config": { … } }` in the body, validates it exactly as create would, and persists nothing — the UI uses it for "test now" before a channel is saved. A transport failure is `422 CHANNEL_TEST_FAILED`. Both count against the `test_now` rate-limit bucket.
- **Platform disables**: when a linked Telegram chat unlinks from its side (the bot is removed, or the chat sends `/stop`), every channel linked to that chat is disabled with a `disabled_reason` the UI shows. Re-enabling the channel clears the note.
- **Platform moves**: when a Telegram group is upgraded to a supergroup its chat id changes and the channel follows without an operator write (`updated_at` moves, `write_source` and verification do not). A `telegram_app` channel moves as soon as the bot's webhook hears of it, for every org linked to the chat; a bring-your-own `telegram` channel moves on the first alert that meets the dead id. A client that diffs configs will see `chat_id` change; the org audit log carries a `channel.chat_migrated` row with `from` and `to`. The unsaved-config test refuses a moved chat with `400 INVALID_CONFIG` naming the new id, since it has nowhere to save it.
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

- `POST /api/v1/notification-channels/delegate` (`channels:write`) with optional `{ "name": "…", "kind": "…" }` hints mints a single-use `/c/<code>` URL (7-day expiry, capped outstanding links per org → `422 DELEGATE_LINK_LIMIT`; a `kind` a delegation link cannot create → `400 DELEGATE_KIND_INVALID`; `telegram_app` only on deployments running the central bot). Only the code's hash is stored.
- `GET /c/<code>` is public and chrome-less: it offers exactly the connect-capable transports of the deployment — the telegram one-tap link + QR (the delegation code doubles as the `t.me` start payload; the bot turns away a code pinned to another kind and leaves it unspent), "add to Slack" / "add to Discord" when the operator OAuth apps are configured, and a manual form for Slack, Discord, Microsoft Teams, Google Chat, email and webhook channels. `POST /c/<code>/create` refuses any other kind with `422 DELEGATE_KIND_INVALID`. The link can create **one** channel in the inviting org and read nothing; expired, revoked, and spent codes all render the same 404 page. Every delegated create lands in the org audit log.
- `GET /api/v1/notification-channels/delegate` (`channels:read`) lists the org's links (`pending` / `consumed` / `expired`); `DELETE /api/v1/notification-channels/delegate/{id}` (`channels:write`) revokes an unconsumed one (revoked links read as expired).

## Rate limiting

`/api/v1/*` is rate-limited per authenticated subject — by `(org, category)` and by `(user, category)`, whichever trips first — with the per-minute budgets taken from the org's plan. Categories: `api_writes` (POST/PATCH/DELETE), `api_reads` (GET/HEAD/OPTIONS), `bulk_ops` (`/bulk*` and `/duplicate`), `test_now` (`/test`), `check_now` (`/check-now`), and `support` (`/support`), the one category with a fixed ceiling on every plan. Exceeding a budget returns `429 Too Many Requests` with a `Retry-After` header (seconds until the next token) and `code: RATE_LIMITED`. `/healthz` and `/readyz` are never throttled. Unauthenticated and per-IP limiting is the reverse proxy's job (see [Deployment](deployment.md)). Full model: [Quotas & rate limits](quotas.md).

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

Common codes: `INVALID_URL_SCHEME`, `INVALID_URL_FORMAT`, `INVALID_ADDRESS`, `SSRF_BLOCKED`, `MAIL_PORT_NOT_ALLOWED`, `INVALID_INTERVAL`, `INVALID_TIMEOUT`, `INVALID_TCP_PORT`, `INVALID_TCP_HOST`, `INVALID_PING_HOST`, `INVALID_HEARTBEAT_PARAMS`, `HEARTBEAT_NOT_PROBEABLE`, `INVALID_MANUAL_STATE`, `MANUAL_NOT_CONFIGURED`, `INVALID_STATUS_RANGE`, `INVALID_TLS_CERT_PARAMS`, `INVALID_DOMAIN_PARAMS`, `INVALID_IP_VERSION`, `INVALID_FLOW_PARAMS`, `FLOW_CHECKS_DISABLED`, `NO_FLOW_CAPABLE_AGENT`, `SMS_ALERTS_DISABLED`, `INVALID_TLS_CRED_COMBO`, `INVALID_ALERT_CONFIG`, `REDACTION_SENTINEL`, `CHECK_KIND_IMMUTABLE`, `BULK_EMPTY`, `BULK_TOO_LARGE`, `BAD_TIME_RANGE`, `TARGET_NOT_FOUND`, `CHANNEL_NOT_FOUND`, `CHANNEL_NAME_TAKEN`, `CHANNEL_NAME_INVALID`, `CHANNEL_QUOTA_EXCEEDED`, `INVALID_CHANNEL_CONFIG`, `CHANNEL_TEST_FAILED`, `CIRCUIT_OPEN`, `DEPENDENCY_DOWN`, `INTERNAL`.

### Quota, rate-limit and abuse codes

| Code | HTTP | Meaning |
|---|---|---|
| `QUOTA_EXCEEDED` | 422 | A plan quota would be exceeded. `details` carries `quota` (e.g. `max_targets`, `max_members`, `max_public_components`), `current`, `limit`, `plan`. |
| `MIN_CHECK_INTERVAL` | 422 | Requested check interval is below the effective floor (`max(plan.min_check_interval_secs, kind_min)`), where `kind_min` is 43200 for `domain_expiry`, 3600 for `tls_cert`, 300 for `flow`, 60 for `heartbeat` and `manual`, and 10 for `http` / `tcp` / `ping` / `dns`. Enforced on create, bulk, **and** PATCH. |
| `INVITATIONS_LIMIT` | 409 | The org is at its pending-invitation cap. |
| `RATE_LIMITED` | 429 | A per-minute rate budget was exceeded. `Retry-After` (seconds) is set; `details.scope` names the tier, e.g. `per_account_api_writes`. |
| `ABUSE_BLOCKED` | 400 | Target blocked by abuse protection. `details.reason` explains. |
| `URL_PATTERN_BLOCKED` | 400 | Target URL matched an abuse pattern (recon path). |
| `DOMAIN_DENYLISTED` | 400 | Target domain (or a parent) is on the deny-list. |
| `BILLING_UNAVAILABLE` | 404 | No payment provider is configured (a self-host build, or the hosted service before one is wired). |
| `PLAN_NOT_FOR_SALE` | 422 | The plan is not sold on the requested cadence (`interval`). |
| `TERMS_NOT_ACCEPTED` | 422 | A checkout was started without `accept_terms: true`. |
| `SUBSCRIPTION_STATE` | 409 | The billing action does not apply to the subscription's current state. |
| `BILLING_PROVIDER_REFUSED` | 409 | The payment provider declined the change; the message carries its reason. |
| `BILLING_PROVIDER_UNREACHABLE` | 503 | The payment provider gave no usable answer. Retry in a moment. |
| `ACCOUNT_OWNER_REQUIRED` | 403 | The caller is in the account but does not own it, so cannot change holds or billing. |

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
- An HTTP result that was not up can carry a `network` object describing what the probe saw on the way, for example `{"remote_ip":"2001:db8::53","hop_url":"https://example.com/login","dns":{"resolver":"1.1.1.1","addresses":["2001:db8::53"],"ttl_secs":2340}}`. Every field is omitted when it does not apply, and the whole object is omitted when none does, including on results recorded before it existed.
  - `remote_ip`: the server address the probe was connected to when the result was decided. Compare it with your server's address: a different one usually means DNS still points somewhere else, for example a public resolver serving an old record until its TTL runs out. Behind a CDN or proxy it is the edge's address, not your origin's.
  - `tried_ips`: on a failed connect, the addresses the probe had for the host. No server answered, so there is no `remote_ip`.
  - `hop_url`: when redirects are followed and a later hop decided the result, that hop's URL, with credential-like query values redacted.
  - `dns`: on a down or error result, the host of the hop that decided it (the check's own host unless a redirect led elsewhere) as the probe's DNS resolver answered it right after the failure, asked past the probe's own cache and at most once a minute per host (sooner when the record's own TTL is shorter, but not more than every 10 seconds). `ttl_secs` is how long, from the result's `timestamp`, that resolver keeps the host's own record before it asks again: for a host that is an alias (CNAME), the alias, which is what moving a hostname usually changes, while the addresses behind it can refresh sooner. Public resolvers run many separate caches, so another query can see a different answer or TTL.
- A `tls_cert`, `dns` or `domain_expiry` result that read something carries `details`, up results included: the same document a result that is not up repeats as its `error`, tagged with the check's `type`, for example `{"type":"tls_cert","days_remaining":84,"not_after":"2027-01-12T10:00:00+00:00","subject_common_name":"example.com","issuer_common_name":"R11"}`. A run that read nothing (a failed handshake, a resolver error, a timeout) and results recorded before the field existed omit it.
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

`uptime_pct` is confirmed incident downtime over the window, weighted as the status page weights it: a partial outage counts for 30% of its length and degraded performance not at all. With `region` set it is that region's share of passing checks instead. It is `null` when the window holds no checks.

## Incidents query

`GET /api/v1/targets/{id}/incidents?from=…&to=…&ongoing_only=false&limit=100&offset=0`

Returns coalesced down / error periods. A contiguous run of bad statuses becomes one incident; an `up` result between two bad runs splits them. Ongoing incidents return `ended_at: null` and `duration_secs: null`. `origin` is `monitor` for an incident the checks opened and `manual` for one an operator declared.

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
      "error_sample": "connection refused",
      "origin":     "monitor"
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

`last_24h.incidents` counts incidents open at any point in the last 24 hours, monitor-opened and declared alike, the same ones the incidents list shows. `uptime_pct` beside it is the share of checks that came back up.

`current_status` counts each monitor once, so its five buckets sum to `targets.total`. A monitor with no readable result in the last 24 hours counts as `unknown`. The state is folded across probe regions under the monitor's region policy, matching the console and the MCP tools: failing regions below the monitor's quorum count as `degraded`, not `down`. See [Multi-region probes](multi-region.md#incident-detection-across-regions).

## On-demand operations

- **`POST /api/v1/targets/test`** — runs one check against a raw `CheckSpec`, no persistence. Same SSRF / URL-scheme / port validation as `POST /targets`. Returns `TestResponse { result, matched_expectations, warnings }`.
- **`POST /api/v1/targets/{id}/check-now`** — runs one check against an existing target using its stored credentials, dispatched to an agent in the target's region. Result is persisted. Returns `503 PROBE_UNAVAILABLE` if no agent is currently serving the region, or `503 PROBE_BUSY` if every agent there is busy running other checks (retry shortly).
- **`POST /api/v1/targets/bulk-action`** — apply one action atomically to up to 10,000 ids. Partial failure allowed; the response lists `succeeded` and `failed` separately, with per-id `code` + `message`.

```jsonc
{
  "ids": ["01h7m...", "01h7n..."],
  "action": { "type": "disable" }
  // alternatives: { "type": "enable" }, { "type": "delete" },
  //   { "type": "tag_add",    "tags": ["frozen"] },
  //   { "type": "tag_remove", "tags": ["frozen"] },
  //   { "type": "set_group",  "group": "tier1" } // null clears the group
  //   { "type": "set_interval",   "interval": 300 }
  //   { "type": "channel_add",    "channel_ids": ["..."] }
  //   { "type": "channel_remove", "channel_ids": ["..."] }
  //   { "type": "set_channels",   "channel_ids": ["..."] } // [] unbinds every channel
}
```

`tag_add` merges into what each monitor already carries, so a target whose merged list would go over the 50-tag limit is left untouched and reported in `failed` as `TOO_MANY_TAGS`. `tag_remove` is not held to the tag rules, so a tag that predates them can still be cleaned up.

`set_interval` below the plan floor fails the whole request with `422 MIN_CHECK_INTERVAL`, as a single edit does. Above it, a monitor whose kind needs a longer interval keeps its own and is reported in `failed` as `MIN_CHECK_INTERVAL`, and heartbeat and manual monitors, whose cadence follows the ping period or is fixed at a minute, are reported as `INVALID_INTERVAL`. `channel_add` and `set_channels` refuse a channel your org does not have with `400 INVALID_ALERT_CONFIG`; `channel_remove` accepts any id. A channel with a tag rule still notifies the monitors carrying its tags after `channel_remove`, since the rule is not a binding.

## Idempotency

`POST /api/v1/targets/bulk`, `POST /api/v1/targets/bulk-action`, `POST /api/v1/targets/{id}/duplicate`, `POST /api/v1/incidents/{id}/updates` and `POST /api/v1/maintenance/{id}/updates` accept an optional `Idempotency-Key` header. Updates cannot be taken back and go to subscribers, so send one key per message. A key is 1 to 255 characters; anything else is `400 INVALID_IDEMPOTENCY_KEY`. The server stores a successful (`2xx`) response for 24 hours keyed by `(caller credential, header value, method and path, body hash)`, where the credential is the bearer token or, in a browser, the session cookie. A retry with the same credential, key and body to the same path, sent after the first request has answered, returns the original response, `Location` included, without re-executing. One sent while the first is still running gets `409 IDEMPOTENCY_IN_PROGRESS`; retry it once the first answers. An error is not stored, so a retry after a `429`, a `5xx` or a refused request runs again. The same key with a different body, or sent to a different path, executes normally. The cache is in-process; entries are lost on restart.

```http
POST /api/v1/targets/bulk-action HTTP/1.1
Idempotency-Key: 01h7m8z4n6v0e1m7v7y6x8x8x8
Content-Type: application/json

{ "ids": ["..."], "action": { "type": "disable" } }
```
