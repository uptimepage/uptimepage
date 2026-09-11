# MCP server

uptimepage exposes a [Model Context Protocol](https://modelcontextprotocol.io/docs/getting-started/intro) server so an LLM client — the [claude.ai](https://claude.ai) connector, Claude Desktop, an IDE, or MCP Inspector — can answer operational questions about **one organization** and take a few guarded actions, through typed, authorized, audited tools.

It is another authorized front door to the same stores the web app and [`/api/v1`](api.md) use, not a bypass: tenant isolation, scopes, rate limits, and audit all apply. Every tool takes the org from the credential — never from a tool argument — so a connection can only ever see and touch its own org.

- **Transport** — Streamable HTTP at `POST/GET /mcp`, served on its own host (`mcp.{DOMAIN}` in production).
- **Auth** — an org-bound scoped API token (`sm_live_…`), minted either by hand (Settings → API tokens) or by the one-click OAuth 2.1 connector flow.
- **Surface** — 16 read tools (14 of them under the default grant; `list_notification_channels` needs `channels:read` and `list_variables` needs `variables:read`) + 15 write tools (each scope-gated, confirmed per action, and audited). Write tools are listed only to clients that can show a confirmation prompt; see [Confirmations](#confirmations).

The server only mounts when enabled (see [Enabling](#enabling)); a deployment that leaves it off never exposes `/mcp`.

## Tools

All tools return typed `structuredContent`. Customer free text (monitor names, group names, tags, error messages, incident text) is returned as **labelled data**, never as instructions to the model — the server's instructions tell the client to treat it that way.

### Read tools

Side-effect-free (`readOnlyHint`). Each requires the scope named in its row: `targets:read`, `status_page:read` and `incidents:read` are in the default grant; `channels:read` and `variables:read` are not.

| Tool | Scope | Returns |
|---|---|---|
| `get_org_health` | `targets:read` | Per-state monitor totals + the worst currently-failing monitors, each with its open `incident_id`. The one-shot "what is broken right now?" answer — start here. |
| `list_monitors` | `targets:read` | Monitors with optional `state` / `type` / `tag` filters, cursor-paginated; each item carries current state + last-checked time. |
| `get_monitor` | `targets:read` | One monitor's **full** config — everything the check asserts (expected status, body match, which request headers are sent, timeout, redirect policy, TLS options, per-kind thresholds) plus the regions it probes from and the ids of the channels it alerts — with its current state, last error, last HTTP status, structured edge-access diagnosis when present, and 24h / 30d uptime. Every field `update_monitor` can change is readable here, so a retune never guesses at what it is changing: `alert_channel_ids` is the read half of `channel_ids` (which replaces the whole set), and `alert_confirmations`, `notify_recovery`, `renotify_interval_secs` and `region_policy` come back individually, `region_policy` in the same `{mode, count}` object the write tools take. Send those fields back, not the whole monitor: `update_monitor` rejects anything outside its allowlist. `managed_externally` says up front that the monitor is Terraform-declared and every write will refuse it. Credentials are never carried: HTTP basic-auth and bearer tokens report only `has_basic_auth` / `has_bearer_token`, header values and the request body come back as `***`, a heartbeat's ping token is withheld, and a flow step's fill value is reported as `value_withheld`. The address is reported as configured, so a credential an operator put in the URL itself is visible there, exactly as in `/api/v1`. |
| `get_monitor_history` | `targets:read` | One monitor's history over a `window` (`1h` / `24h` / `7d` / `30d`): uptime, latency series, a per-region split of the same window, failures with error text, incident windows. `region` narrows uptime, latency, and the split to one probe region, which is how "down everywhere or only from Singapore?" gets answered. |
| `list_regions` | `targets:read` | The fleet's probe regions: id, display name, city, country, continent, and `default_selected` — whether a new monitor probes from there unless told otherwise. Reports `max_regions` alongside, but only where the plan reaches fewer regions than the catalog lists, so a set too large to be accepted is visible before it is sent and a plan that reaches everything advertises no ceiling to fill. The id is what `get_monitor.regions` lists and what `get_monitor_history` takes as `region`. |
| `list_tags` | `targets:read` | Every tag in use across the org's monitors, most-used first, with a monitor count each. `list_monitors` filters by an exact tag; this is how to learn which ones exist. Says so when the inventory is truncated. |
| `get_flow_runs` | `targets:read` | A browser flow monitor's recent runs over a `window`: every declared step with its outcome and duration, the step a failure stopped on, and the page the browser saw. Answers *why* a login check failed. |
| `get_flow_step_trend` | `targets:read` | Per step of a browser flow monitor, over a `window`: earliest and latest mean duration, the ratio between them, and how many runs passed or failed it. Answers *which step is getting slower* while the monitor still reports up. |
| `list_incidents` | `incidents:read` | Incidents with their id, affected monitor, severity, open/resolved times, and latest update phase. Defaults to the currently-open ones; `state: "all"` adds resolved history inside a `from`/`to` window (default: the last 30 days, capped at a year), and `monitor_id` narrows to one monitor. The response repeats the window it actually read, so a clamped request is never reported as the span that was asked for. An incident still running is listed however long ago it opened. Cursor-paginated. |
| `get_incident` | `incidents:read` | One incident: affected monitor, severity, open/resolved times, error sample, and the full operator-update timeline. |
| `get_incident_metrics` | `incidents:read` | Incident metrics over a trailing window (default 30 days): MTTA/MTTR, total, counts by severity and state, auto- vs human-resolved, and the noisiest monitors. |
| `list_status_pages` | `status_page:read` | The org's status pages: slug, name, public URL, enabled. Cursor-paginated. |
| `get_status_page` | `status_page:read` | One status page with its components and each linked monitor's current state. |
| `get_org_usage` | `targets:read` | Which org the connector is bound to (slug and name), and resource usage against plan limits (monitors, status pages, members, components) + key policy values. A token carries one org and cannot switch, so this is the one-call answer to "where am I connected?" — see [Org binding](#org-binding). |
| `list_variables` | `variables:read` | The org's [variables](variables.md): key and whether it is a secret. Values are never returned, and a secret's is never even read. This is how a caller finds the key to write as `{{ key }}` when building an authenticated check. Variables are created and edited in the app, never here — `variables:write` is not grantable to a connector. |
| `list_notification_channels` | `channels:read` | The org's notification channels: id, operator-set name, kind, enabled, plus two flags for a channel that is not working even where it reads as ready: `awaiting_verification` for an email address that was never confirmed, and `not_delivering` for an enabled channel whose recent alerts all failed to arrive. The settings that make a channel work (webhook URLs, bot tokens, addresses) are withheld. Channels are created in the app, never here. `channels:read` is not one of the default connector scopes: a client that never touches alerting is not offered the inventory. |

A `window` is a request, not a promise: every history tool clamps it to what your plan retains at per-check detail, and one clamp covers the whole response so its fields never describe different spans. See [Quotas and limits](quotas.md).

`get_monitor_history` measures uptime the way the rest of the product does: unfiltered, it counts *confirmed* incident time, so a flap that never breached the alert threshold does not dent it, and neither does an incident an operator declared by hand unless they asked for it to count. Each entry in `incidents` carries `counts_as_downtime` so a window listed there can be told apart from one that explains the `uptime` gap. Under a `region` filter there is no such thing as a confirmed regional incident, so `uptime` becomes that region's raw pass rate — the two numbers answer different questions and should not be compared. `failures` and `incidents` always describe the monitor as a whole, since an incident is raised for the monitor rather than per region; the `regions` split is what shows a partial outage. That split is empty for a monitor that runs in one region, because one region cannot disagree with itself.

A status-page monitor is down → `get_org_health` gives the `incident_id` → `get_incident` shows the timeline → `acknowledge_incident` takes ownership → `publish_incident` puts it on the status page → `post_incident_update` tells your customers. Incidents (and the `incident_id` / ack workflow) exist only for monitors that are status-page components; a monitor not on any status page can be failing with `incident_id: null` — `since` still reports how long it's been down. `run_check_now` and `get_monitor` return `http_status` for HTTP monitors so you can tell "wrong status code" from "no response".

### Write tools

Not read-only. Each requires its scope **and** an interactive [confirmation](#confirmations) before it runs, and writes exactly one [audit](#audit) row for every outcome (success, declined, denied, error).

| Tool | Scope | Effect |
|---|---|---|
| `create_monitor` | `targets:write` + `targets:execute` (+ `channels:read` to bind channels) | Create an `http`, `tcp`, `ping`, `dns`, `tls_cert`, `domain_expiry` or `heartbeat` monitor, optionally naming the `regions` it probes from. See [How creation is guarded](#how-creation-is-guarded). |
| `create_monitors` | same as `create_monitor` | Create several monitors under **one** confirmation. Every check is trial-run first and all the results are shown together; an item that fails validation or its probe is reported in the results and the rest are still created. Same per-monitor fields and same guards as `create_monitor`. Prefer it whenever the user names more than one thing to watch — ten monitors otherwise costs ten prompts. |
| `create_status_page` | `status_page:write` + org owner | Create a status page from a `slug` and `name`. Created unpublished unless `enabled` is passed, so its components can be curated before anyone can read it. The slug is the page's public address, first-come across the platform, and moving it later breaks every existing link. |
| `update_status_page` | `status_page:write` + org owner | Rename a page, move it to a new slug, or publish and unpublish it. An omitted field is left alone. Idempotent. |
| `add_status_page_components` | `status_page:write` + org owner | Put monitors on a page as public components, in one confirmation. Each takes a `public_name` (the monitor's own name is operator-facing), an optional `public_group` to file related components together, and `detail_link_enabled` to publish a per-monitor detail view. A monitor already on the page is reported as such, never duplicated. |
| `update_status_page_component` | `status_page:write` + org owner | Change how one monitor is presented on a page: public name, description, group, sort order. An omitted field is left alone. A detail link is set when the component is added. Idempotent. |
| `run_check_now` | `targets:execute` | Probe a monitor immediately and record the result. A `down` result may fire the org's normal alerts. A heartbeat monitor has nothing to probe and is refused as `invalid_argument`, not as something to retry. |
| `update_monitor` | `targets:write` (+ `channels:read` to rebind channels) | Change how loudly a monitor is watched: `interval_secs`, `alert_confirmations`, `notify_recovery`, `renotify_interval_secs`, `tags`, `group_name`, `region_policy`, `channel_ids`. Nothing else — see [What it will not change](#what-update-monitor-will-not-change). `tags` replaces the whole list and takes at most 50, each at most 50 characters, with no blank and no invisible characters. The confirmation names the monitor and states old → new for every field, and a request whose values already match writes nothing and never prompts. If the monitor moves between the prompt and the approval, the write is refused as `conflict` instead of landing on top of the newer value. Idempotent. |
| `pause_monitor` | `targets:write` | Stop a monitor's checks until resumed. Idempotent. |
| `resume_monitor` | `targets:write` | Restart a paused monitor's checks. Idempotent. |
| `acknowledge_incident` | `incidents:write` | Take ownership of an incident and halt escalation. Internal only: it posts nothing to the public status page. Idempotent. |
| `resolve_incident` | `incidents:write` | Mark the incident resolved. Internal only, same as acknowledge: the public page is untouched. Idempotent. |
| `publish_incident` | `incidents:write` | Put an incident on every status page carrying the affected monitor, optionally seeding `public_title` and `public_description`. Subscribers may be notified. Idempotent. |
| `unpublish_incident` | `incidents:write` | Take a published incident back off the public pages. The operator timeline is untouched. Idempotent. |
| `post_incident_update` | `incidents:write` | Post the customer-facing update to the incident's status-page timeline: a `message` plus optional `phase` (`investigating` / `identified` / `monitoring` / `resolved` / `postmortem`, default `investigating`). Requires a published incident. |

Incidents start internal, so the customer-facing sequence is `publish_incident` then `post_incident_update`; posting to an unpublished incident is refused rather than written somewhere nobody reads.

### How creation is guarded

`create_monitor` is the one tool that brings something into existence, so it carries three constraints the others don't need.

**The check runs before anything is saved.** The trial result — the region it ran from, passed or not, HTTP status, duration, or the error text — is part of the confirmation the operator reads, so a check that asserts the wrong thing is visible while it can still be declined rather than after it starts paging. That is also why the tool needs `targets:execute` alongside `targets:write`: it dispatches a real probe at a caller-supplied address, and that probe is metered against the same `test_now` budget as `POST /targets/test`. If no agent is serving the region, creation is refused as `probe_unavailable` rather than persisted untried, since a monitor nothing can check is not worth having.

Two consequences worth stating plainly. The probe necessarily happens **before** the human answers, so declining the prompt still means one request was made to that address — the confirmation decides whether the monitor is created, not whether the check was tried. And a client that never negotiated elicitation is refused *before* the probe, so it cannot use this tool to reach an address it chooses without ever being able to create anything.

**The confirmation lists every setting**, not just the address: interval, probe regions, tags, group, how many failing checks it alerts after, whether recovery is announced, the reminder cadence, and the multi-region quorum, resolved against the regions the monitor is being given rather than left as a label a narrower assignment would clamp. A field the prompt omitted would be approved unread.

**A credential is referenced, never pasted.** Request headers and a request body can be set, which is what makes an authenticated check — a `POST /v1/chat/completions` carrying an API key — expressible here at all. What cannot be done is spelling the secret out. A header whose name carries a credential (`authorization`, `proxy-authorization`, `x-api-key`, `api-key`, `cookie`) must hold exactly a [variable](variables.md) reference, optionally behind a scheme word: `Bearer {{ openai_key }}`, or a bare `{{ session_cookie }}`. Anything looser is refused, pointing at `list_variables` for the keys the org has.

The reason is where the value would come to rest. A pasted key is echoed back in the tool result and lives on in the chat transcript, and it is stored in the monitor's `check_spec` as plaintext, where a variable would have been sealed at rest — so it also has to be re-pasted into every monitor that needs it instead of being rotated in one place. Referencing costs nothing at probe time: the reference resolves just before the check runs, exactly as it does for a monitor built in the app.

This is a hygiene rule, not a security boundary: it gates the five header names a credential normally travels under, and a caller determined to paste a key under some other name can. What it stops is the accident, which is the common case. Basic auth, bearer tokens and browser flows are still app-only — a flow's fill values are withheld from every other MCP tool, and `update_monitor` refuses to change a URL, header or body on a monitor that already exists.

**Channels are bound by id, never created.** `channel_ids` on `create_monitor` (and on `update_monitor`, which replaces the whole set) binds channels that already exist; `list_notification_channels` is how their ids are found, and `channels:read` is required to use them, since naming and validating them means reading the inventory. A channel id from another org is refused, as is one that does not exist. The channel itself — its webhook URL, bot token or address — is only ever set up in the app. Omit `channel_ids` and the monitor alerts nobody, which is the failure this tool is likeliest to leave behind: monitors that look right in the console and page no one at 3am. The tool description and the server instructions both push the caller to bind as it creates, and to say plainly that nothing is bound when the org has no channel yet or the token lacks `channels:read`.

The confirmation names the channels rather than listing ids, and says outright when one is disabled or is an email address that was never verified, since either delivers nothing.

**A vague ask gets a named starter set.** "Add monitoring on my SaaS" carries one URL and no check list, so the server instructions name the set worth creating from it: an `http` check on the public URL, a `tls_cert` check on its host, and a `domain_expiry` check on its domain. The last two are there because a certificate or a registration lapsing is the outage that arrives with no warning and no bad deploy to blame, and because they are the two a caller working from a single URL would not think to ask for. Anything past that waits for an endpoint the user names.

The interval defaults to where the app's own picker opens a monitor of that kind, held up to the plan's floor — not to the hard minimum, which is legal but would probe a certificate twelve times more often than any other front door does. For a heartbeat it is capped at `period + grace`, since a tick coarser than the window could never judge it.

**The default region set is the answer, not a fallback.** `regions` takes ids from `list_regions` and pins where the check runs from, and omitting it takes the operator's default-on regions, capped at the plan's `max_regions` and falling back to the control plane's own region when nothing is flagged. `list_regions` reports `default_selected` against that *resolved* set rather than the raw column, so a plan whose cap bites is never told it will get coverage creation would trim away. Its `max_regions` is reported only when the plan reaches fewer regions than the catalog holds: on every plan but free the cap is effectively unbounded, and a ceiling that happens to equal the number of rows returned reads as leave to name all of them. A caller reading the catalog sees a list of places and a cap, not a form to fill: naming all of them is the wrong instinct, and on a plan whose `max_regions` is smaller it fails outright rather than trimming to fit. A vantage point can be published without being ticked by default, so a monitor that needs one has to name it. The set is vetted before the trial probe, so an unknown or disabled id, or one the plan's region cap will not pay for, is refused without spending a probe. A heartbeat is pinged rather than probed and rejects the argument outright. The result reports the regions the monitor was actually assigned, so the default set never has to be guessed at.

Beyond that, a monitor created here gets the same heartbeat ping row and immediate first check that `POST /targets` gives it, because both doors go through one creation path. Without that a heartbeat would have no ping URL to call, and a multi-region plan would quietly get single-region monitors.

### What `update_monitor` will not change

The line is drawn on one rule: **how loudly we watch** is editable, **what we watch** is not. Name, address or URL, assertions, expected status, request headers and body, probe regions and owner are all refused, and passing one is an error rather than a silent no-op. Which channels a monitor alerts *is* editable, by id: that changes who hears about an outage, not whether one is detected.

This is not squeamishness about writes. Changing the interval or the confirmation count can only make alerting noisier or quieter, shows up on the next check, and is trivially reversible — that belongs in a conversation held during an incident. Changing what a check asserts can make a broken service report healthy, and that failure is silent and open-ended: nobody gets paged, and the monitor keeps saying everything is fine. That belongs in config-as-code, where it gets a diff and a review.

### Terraform-managed monitors

A monitor whose `write_source` is `terraform` is refused by `update_monitor`, `pause_monitor` and `resume_monitor` with `managed_externally`, naming the monitor and pointing at the `.tf` that declares it. There is no override argument. Without the guard the edit lands and the next `terraform apply` reverts it, which no confirmation prompt can warn about because the prompt is answered long before the revert.

MCP writes also leave `write_source` as they found it, rather than restamping it. Otherwise the first MCP write would erase the `terraform` marker and the guard would protect exactly one edit. Attribution for MCP writes lives in the [audit](#audit) log, which records the token, the user, and the tool for every outcome.

Write scopes are **never** granted unless explicitly requested — the OAuth connector defaults to read-only (see [Scopes](#scopes)).

## Authentication

The `/mcp` endpoint is an OAuth 2.1 protected resource. It accepts an `Authorization: Bearer sm_live_…` token that must be:

- a live scoped [API token](authentication.md#api-token-auth),
- **bound to one org** (an unbound token is rejected — the connection has no org header to fall back on), held by a current member of that org,
- carrying the scope each tool requires (else `403 insufficient_scope`), and
- when OAuth is configured, stamped with this endpoint as its `audience` (RFC 8707) — a token minted for a different audience is refused.

A request with no/invalid token gets `401` with a `WWW-Authenticate: Bearer …` header pointing at the resource metadata, which kicks off discovery for OAuth clients.

### Two ways to get a token

**1. By hand (manual connector).** Mint an org-bound, read-only, expiring token in the UI (Settings → API tokens; a verified email is required) and paste it into the client. Grant the least scope you need — `targets:read` + `status_page:read` + `incidents:read` for the read tools. This is the simplest path for Claude Desktop / Inspector and needs only `UPTIMEPAGE_MCP_ENABLED`.

Either way, the token only decides what the connection *may* do. Whether the write tools appear at all depends on the client: one that can't prompt for confirmation is offered the read tools only. See [Confirmations](#confirmations).

**2. One-click OAuth (claude.ai connector).** With `UPTIMEPAGE_MCP_OAUTH_ENABLED` on, the client discovers the authorization server, you log in with your existing session and approve a consent screen, and the server mints the same org-bound expiring token behind the scenes — no copy-paste. This is the only path that mints write scopes, and only the ones the client asked for: the consent screen lists exactly what is about to be granted and approval covers that whole set, so a client asking for more than it needs is declined as a whole rather than trimmed.

### Why OAuth at all?

The manual path works but pushes a long-lived bearer token through copy-paste and client config. OAuth replaces that with a browser consent: the user authenticates against the existing login, the connector receives a short-lived access token plus a rotating refresh token, and the **connection lifetime** (refresh-token lifetime) is the user's explicit choice on the consent screen (default 90 days, max 365 — there is deliberately no "never"). Reused refresh tokens revoke the whole family. The connector never sees the user's password and the access token is bound to this one resource.

### OAuth endpoints

Discovery + authorization-server endpoints live on the **app** host (where the session cookie lives); the protected resource is `/mcp` on its own host.

| Endpoint | Host | Purpose |
|---|---|---|
| `/.well-known/oauth-protected-resource` | resource (`mcp.`) | RFC 9728 resource metadata (resource id, authorization servers, scopes) |
| `/.well-known/oauth-authorization-server` | app | RFC 8414 AS metadata (PKCE S256 only, public clients, code + refresh grants) |
| `/oauth/register` | app | RFC 7591 Dynamic Client Registration |
| `/oauth/authorize` | app | Login + consent screen (PKCE S256, RFC 8707 `resource`) |
| `/oauth/token` | app | Issue / refresh the audience-bound token |

Redirect URIs may be HTTPS hosts (web connectors), loopback HTTP including `[::1]` (mcp-remote, VS Code), or a native app's own scheme: the reverse-DNS form RFC 8252 asks for (`com.example.app:/cb`) or `cursor://`, the one editor scheme admitted by name. Everything else is rejected at registration: non-loopback cleartext, any other protocol handler, userinfo, fragments. The list is an allowlist because the authorize error path redirects to the registered URI before login. PKCE is mandatory, which is what makes a native scheme safe once admitted: a program that hijacks the scheme on the user's machine receives the code but cannot redeem it.

### Consent screen

`GET /oauth/authorize` renders the consent screen — the one page the user sees during the OAuth flow. It appears **after login**, once the client + redirect URI are validated, and only when `mcp.oauth_enabled` is on. Approving here is what mints the token; nothing is granted until the user clicks Approve.

It shows:

- **Who and what** — the client name and the single org it's connecting to. Access is always scoped to that one org.
- **Granted abilities** — one line per scope, in plain language (e.g. "Read your monitors and their current status", "Pause and resume your monitors"). Write abilities are flagged with a ⚠ marker, and a warning banner appears at the top stating the connection can make changes — each of which still asks for per-action confirmation.
- **Connection expires** — a picker (30 / 60 / 90 / 365 days, default 90) that sets the refresh-token (connection) lifetime. There is no "never".
- **Approve / Deny** — Deny aborts the flow; Approve mints the org-bound scoped token and returns the user to the client.

A read-only request shows "wants read-only access" with no warning banner; a request that includes any write scope switches to the "is requesting access" wording plus the banner and ⚠ markers.

## Scopes

The connector advertises nine grantable scopes. A request with no `scope` (or only unknown scopes) grants the **read-only default**; everything else is opt-in.

| Scope | Grants | In default set? |
|---|---|---|
| `targets:read` | all read tools over monitors | ✅ |
| `status_page:read` | status-page read tools | ✅ |
| `incidents:read` | `list_incidents`, `get_incident`, `get_incident_metrics` | ✅ |
| `channels:read` | `list_notification_channels`, and binding channels on `create_monitor` / `update_monitor` | opt-in |
| `targets:write` | `create_monitor`, `create_monitors`, `update_monitor`, `pause_monitor`, `resume_monitor` | opt-in |
| `targets:execute` | `run_check_now`, and the trial probes `create_monitor` / `create_monitors` run | opt-in |
| `incidents:write` | `acknowledge_incident`, `resolve_incident`, `publish_incident`, `unpublish_incident`, `post_incident_update` | opt-in |
| `status_page:write` | `create_status_page`, `update_status_page`, `add_status_page_components`, `update_status_page_component` — **and** the caller must be an owner of the org, the same bar `/api/v1` holds the public brand surface to | opt-in |
| `variables:read` | `list_variables` — variable keys only, never a value | opt-in |

A granted write scope is **necessary but not sufficient** — every write tool still asks the user to confirm the specific action at call time.

`variables:write` is deliberately absent. Creating or rotating a variable means carrying its value, which is the one thing this surface will not do; variables are managed in the app or over [`/api/v1`](api.md#operator-endpoints-variables).

## Org binding

A token is bound to **one** org, and there is no tool argument that switches it. The binding is set when the token is minted, from whichever org was active in the app at that moment — the OAuth consent screen displays it but does not offer a picker, and the scope list there is display-only too.

So creating a new org in the app does not make it visible to an existing connection, and reconnecting while the app still has the old org active mints another token on the **old** org. The order that works: switch to the org you want in the app, confirm the consent screen names it, then approve. `get_org_usage` reports the bound org, which is the fastest way to check before wondering why a monitor is missing.

## Confirmations

Before any write tool acts, the server sends an MCP **elicitation** request describing the exact action: which monitor or incident it lands on, the effect, and for a public update the text customers will read. Nothing is ever confirmed unnamed, so an approval can't be steered onto a different incident than the one under discussion. The tool proceeds only on an explicit approval. A decline or a dismissal fails closed with `not_confirmed`. If the client never answers at all — it times out, sends nothing, or sends something unreadable — that fails closed too, under the separate code `confirmation_failed`, so `not_confirmed` always means a person decided. There is no "remember my choice", so each action is confirmed on its own.

**Your client must support elicitation to use any write tool.** One that doesn't is offered the read tools only: the write tools are left out of its `tools/list`, since every one of them would refuse. If such a client calls a write tool anyway it gets `elicitation_unsupported`, which is a distinct code from `not_confirmed` so "your client can't ask" never reads as "you said no". A client that supports only url-mode elicitation counts as one that can't ask, since the confirmation is a form. Nothing about this weakens the guarantee: hiding is presentation, and the confirmation itself is what makes a write safe.

## Audit

Every write-tool invocation writes one row to `mcp_audit`, on **every** path — success, user-declined, scope-denied, bad input, not-found, or server error — recording: `actor_type = mcp`, the token id, the acting user + org, the tool name, what it acted on, the outcome (`success` / `denied` / `error`), and a detail that leads with the refusal code and adds its reason where there is one (`not_confirmed:declined`, `confirmation_failed:timed_out`). "What it acted on" is the id for most tools, the created monitor's name, address, interval and bound channels for `create_monitor`, and the old → new pairs for `update_monitor`; a refused call records only what identifies the attempt. Customer-facing incident text is never recorded — not a public title, a description, an update `message`, nor an ack/resolve note. The same event is emitted to tracing. Reads are not audit-logged (they're side-effect-free and already rate-limited). Rows are kept for `retention.mcp_audit_days` (2 years by default), then deleted by the daily retention job.

## Enabling

Off by default. Config keys (TOML under `[mcp]`, or env with the `UPTIMEPAGE_` prefix and `__` nested separator):

| Key | Env | Default | Purpose |
|---|---|---|---|
| `mcp.enabled` | `UPTIMEPAGE_MCP__ENABLED` | `false` | Mount `/mcp` + the read/write tools. |
| `mcp.oauth_enabled` | `UPTIMEPAGE_MCP__OAUTH_ENABLED` | `false` | Add the OAuth 2.1 endpoints that back the one-click connector. |
| `mcp.resource_uri` | `UPTIMEPAGE_MCP__RESOURCE_URI` | _empty_ | Canonical absolute URI of `/mcp` — the OAuth resource id + RFC 8707 audience, e.g. `https://mcp.uptimepage.dev/mcp`. Empty disables audience binding (static-token mode). |
| `mcp.allowed_origins` | `UPTIMEPAGE_MCP__ALLOWED_ORIGINS` | _empty_ | RFC 6454 Origin allow-list (DNS-rebinding defense). Empty disables the check; a missing `Origin` header always passes (non-browser clients send none). |
| `mcp.access_token_ttl_secs` | `UPTIMEPAGE_MCP__ACCESS_TOKEN_TTL_SECS` | `3600` | Access-token lifetime (short; auto-renewed via the rotating refresh token). |

When OAuth is on, the app **refuses to boot** unless `mcp.resource_uri` and `auth.public_base_url` are real HTTPS origins — the issuer and audience must be well-formed. Migrations `016` (OAuth) + `017` (audit) must be applied.

### Production (GitHub-managed)

The deploy pipeline upserts the two switches from repo **variables** (Settings → Secrets and variables → Actions → Variables):

- `MCP_ENABLED=true`
- `MCP_OAUTH_ENABLED=true`

`deploy.yml` writes the corresponding `UPTIMEPAGE_MCP_*` keys into the server `.env` on each deploy. The resource URI defaults to `https://mcp.{UPTIMEPAGE_DOMAIN}/mcp`; `mcp.{DOMAIN}` rides the existing `*.{DOMAIN}` wildcard cert + Caddy route (no new DNS). See `deployment/.env.example` and [Deployment](deployment.md).

## Connecting a client

### claude.ai connector (OAuth)

Settings → Connectors → Add custom connector → URL `https://mcp.{DOMAIN}/mcp` → Connect. You'll be sent to the login + consent screen; approve, and the tools appear. This exercises the full OAuth path and is the recommended end-user flow.

### Cursor, VS Code (OAuth)

Add the server by URL, `https://mcp.{DOMAIN}/mcp`, in the editor's MCP settings. The editor registers itself, opens the login + consent screen in your browser, and returns to the editor once you approve (Cursor on its `cursor://` scheme, VS Code on a loopback port). No token to paste.

### Claude Desktop / IDE (manual token via mcp-remote)

`mcp-remote` bridges a local stdio client to the remote Streamable HTTP endpoint. Add to your client config:

```jsonc
{
  "mcpServers": {
    "uptimepage": {
      "command": "npx",
      "args": [
        "-y", "mcp-remote",
        "https://mcp.uptimepage.dev/mcp",
        "--header", "Authorization: Bearer sm_live_YOUR_TOKEN"
      ]
    }
  }
}
```

For a local dev server over plain HTTP, add `--allow-http` to the args.

### MCP Inspector (testing)

```bash
npx @modelcontextprotocol/inspector
```

Set transport **Streamable HTTP**, URL `https://mcp.uptimepage.dev/mcp`, and an `Authorization: Bearer sm_live_…` header. Inspector lists every tool with its schema and lets you exercise the elicitation approve/deny flow.

## Examples

### Raw protocol (curl)

The transport is JSON-RPC over Streamable HTTP. `initialize` returns a session id the client echoes on later calls.

```bash
# initialize → 200 + Mcp-Session-Id response header
curl -sD- https://mcp.uptimepage.dev/mcp \
  -H 'Authorization: Bearer sm_live_YOUR_TOKEN' \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize",
       "params":{"protocolVersion":"2025-11-25",
                 "capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'

# list tools (reuse the session id from the initialize response)
curl -s https://mcp.uptimepage.dev/mcp \
  -H 'Authorization: Bearer sm_live_YOUR_TOKEN' \
  -H 'Mcp-Session-Id: THE_SESSION_ID' \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'

# call a tool: open incidents on your status pages
curl -s https://mcp.uptimepage.dev/mcp \
  -H 'Authorization: Bearer sm_live_YOUR_TOKEN' \
  -H 'Mcp-Session-Id: THE_SESSION_ID' \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call",
       "params":{"name":"list_incidents","arguments":{}}}'

# read one incident's timeline (id from list_incidents or get_org_health)
curl -s https://mcp.uptimepage.dev/mcp \
  -H 'Authorization: Bearer sm_live_YOUR_TOKEN' \
  -H 'Mcp-Session-Id: THE_SESSION_ID' \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":4,"method":"tools/call",
       "params":{"name":"get_incident","arguments":{"id":"INCIDENT_ID"}}}'
```

Write tools (`acknowledge_incident`, `pause_monitor`, …) follow the same `tools/call` shape but the client must support [elicitation](#confirmations). curl declares no such capability, so it won't see them in `tools/list` and gets `elicitation_unsupported` if it calls one anyway. Drive them from a real MCP client.

A missing/invalid token returns `401` with `WWW-Authenticate: Bearer …`; a wrong `Host` returns `403`; a missing `MCP-Protocol-Version` on a non-initialize call returns `400`; notifications get `202`.

### Asking an LLM

Once connected, drive it in natural language — the client picks the tool:

- "What's broken in my org right now?" → `get_org_health`
- "Show me every DNS monitor that's degraded." → `list_monitors(type=dns, state=degraded)` (`state` is folded across probe regions under the monitor's region policy, so `degraded` covers both a degraded check result and a failure in too few regions to count as down)
- "How has the checkout API done over the last 7 days?" → `get_monitor_history(window=7d)`
- "Is it down everywhere or only from Singapore?" → `get_monitor_history(window=24h)`, then `region=apac-sg`
- "Why does this check treat a 301 as a failure?" → `get_monitor` (reads `follow_redirects` + `expected_status`)
- "Which tags am I using?" → `list_tags` → `list_monitors(tag=…)`
- "Why did the login check fail last night?" → `get_flow_runs(window=24h)`
- "Is any step of the sign-in getting slower?" → `get_flow_step_trend(window=30d)`
- "What incidents are open, and what's been posted on them?" → `list_incidents` → `get_incident`
- "What broke last week?" → `list_incidents(state=all, from=…, to=…)`
- "How often did checkout fail this month?" → `list_incidents(state=all, monitor_id=…)`
- "Acknowledge the payments incident — we're investigating." → `acknowledge_incident(phase=investigating)` (asks you to confirm)
- "Put the payments outage on our status page." → `publish_incident` (asks you to confirm)
- "Am I near any plan limits?" → `get_org_usage`
- "Run a check on the payments monitor now." → `run_check_now` (asks you to confirm; may alert)
- "Pause the staging monitor." → `pause_monitor` (asks you to confirm)
- "Checkout is flapping — make it wait for three failures before paging." → `update_monitor(alert_confirmations=3)` (asks you to confirm, showing 2 → 3)
- "Stop paging until two regions agree it's down." → `update_monitor(region_policy={mode:"count",count:2})` (asks you to confirm)
- "Watch https://shop.example.com and page me if it stops returning 200." → `create_monitor` (runs the check once, shows you the result, asks you to confirm)
- "Set up monitors for the endpoints in this repo." → the model reads the project, then calls `create_monitor` per endpoint, each with its own trial run and confirmation

## Security model

- **Org isolation.** Org comes from the token, never an argument; the token must be org-bound and the holder a live member. The cross-tenant guarantees in [Multi-tenancy](multi-tenancy.md) apply unchanged.
- **Least privilege.** Read-only by default; write scopes are opt-in and each write is separately confirmed and audited.
- **Audience binding.** With OAuth on, tokens are pinned to this `/mcp` resource (RFC 8707), so a token leaked from elsewhere can't be replayed here.
- **DNS-rebinding defense.** The transport enforces a Host allow-list (the configured resource host) and an optional Origin allow-list.
- **Prompt-injection posture.** Customer-supplied text is returned as labelled data and the server instructions tell the client not to treat it as commands — but the ultimate guard is that the dangerous tools are scope-gated and human-confirmed.

## Related

- [Authentication](authentication.md) — scoped API tokens, org binding, expiry.
- [Multi-tenancy](multi-tenancy.md) — the isolation model every tool inherits.
- [Quotas & rate limits](quotas.md) — the per-plan limiter `/mcp` shares.
- [Configuration](configuration.md) — full config reference.
