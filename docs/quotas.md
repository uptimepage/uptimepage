# Quotas & rate limits

Every organization belongs to an **account**, and the account is bound to a
**plan**. The plan is the single source of truth for resource quotas and
per-minute rate budgets — the number a request is enforced at is the same
number the API reports back. A new tier is one row in the `plans` table;
nothing in the enforcement path changes.

The account is the quota subject, not the org. Every count below spans all of
the account's live orgs, so a second org is a workspace over the same pool, not
a second allowance — and a member invited into someone else's org brings access
only, never capacity. `max_orgs` bounds how many workspaces the pool is split
across. Soft-deleted orgs are outside the pool while they wait out the deletion
grace window (their monitoring is paused), which is why restoring one re-checks
`max_orgs` the same way creating one does.

## The seeded plans

Four plans ship seeded: `free`, `founding` (a more generous
free tier granted to early accounts and kept for life), `pro`, and `team`. Only
`free` is listed; the others are assigned by signup, by billing, or by the
self-host boot seed. On the hosted service `free` is sold as **Standard** and
the rest carry their own names; see the
[pricing page](https://uptimepage.dev/pricing).

| Quota | free | founding | pro | team | Meaning |
|---|---|---|---|---|---|
| `max_orgs` | 1 | 3 | 5 | 10 | Organizations the account may hold. They share every other quota on this table |
| `max_targets` | 20 | 50 | 50 | 150 | Monitored targets across the account's orgs |
| `min_check_interval_secs` | 180 | 60 | 60 | 30 | Plan-side floor on a target's check interval. The effective floor is `max(this, kind_min)` — `kind_min` is 43200 for `domain_expiry`, 3600 for `tls_cert`, 300 for `flow`, 60 for `heartbeat`, and 10 for `http` / `tcp` / `dns` / `ping`. |
| `retention_days` | 30 | 90 | 90 | 395 | History window the UI and API will read |
| `raw_days` | 30 | 30 | 30 | 30 | Per-check detail retention, stamped onto each ClickHouse row at write time |
| `evidence_days` | 7 | 7 | 7 | 7 | How long a failed browser-flow run keeps the page it captured. Clamped to `raw_days`, since the run it explains goes then |
| `max_flow_steps` | 30 | 30 | 30 | 30 | Steps one flow monitor may declare. Clamped to the engine ceiling of 30, so a larger value has no effect |
| `max_flow_checks` | 0 | 1 | 3 | 10 | Browser flow monitors the org can create; 0 doubles as the feature gate, so a create returns `403 FLOW_CHECKS_DISABLED` rather than a quota error. A flow also needs `flow.enabled` on the process that runs it, so a cap alone does not switch it on |
| `max_regions` | 3 | ∞ | ∞ | ∞ | Regions a single monitor is probed from. Enforced on assignment and again at hand-out: past the cap, a monitor's default-on regions come first, then the rest by id |
| `max_members` | 3 | 5 | 5 | 15 | Distinct people across the account's orgs — one person in two orgs holds one seat |
| `max_pending_invitations` | 10 | 15 | 15 | 25 | Outstanding (unaccepted) invitations |
| `max_api_tokens_per_user` | 5 | 7 | 7 | 10 | API tokens a single user may hold |
| `max_status_pages` | 1 | 2 | 2 | 5 | Public status pages across the account's orgs |
| `max_public_components` | 15 | 30 | 30 | 75 | Distinct monitors published across all of the account's pages (a monitor on several pages counts once) |
| `max_share_links_per_monitor` | 1 | 3 | 3 | 5 | Live share links on one monitor |
| `max_shared_monitors` | 2 | 5 | 5 | 10 | Monitors with at least one share link |
| `max_maintenance_windows` | 20 | 30 | 30 | 50 | Scheduled maintenance windows |
| `max_notification_channels` | 20 | 30 | 30 | 50 | Notification channels (Slack/webhook/Telegram/WhatsApp/SMS/…) across the account's orgs |
| `max_escalation_policies` | 0 | 0 | 0 | 50 | Escalation policies. On-call and escalation are a `team` feature |
| `max_on_call_schedules` | 0 | 0 | 0 | 25 | On-call schedules |
| `max_logo_size_bytes` | 1048576 | 1048576 | 1048576 | 1048576 | Status-page logo upload ceiling (1 MiB) |

Feature flags ride on the same row: `custom_domain_enabled`, `white_label_enabled`,
`sms_alerts_enabled`, `incident_narration_enabled`, `on_call_enabled`.
`custom_domain_enabled` gates the origin subscriber mail and webhooks link to:
on a plan without it, a verified custom domain is ignored and those links point
at the page's own subdomain.
`white_label_enabled` is what makes the status-page "powered by" toggle real:
on a plan without it the badge always renders, whatever the page setting says.
The same flag decides whether the header's `public_website_url` link passes
ranking signal — followed on a plan that sells white-label, `rel="nofollow"`
everywhere else, since open signup makes a free page's outbound link worth
spamming for. A self-hosted deployment has no open signup, so it follows the
link on any plan, and a page with `public_hide_from_search` set follows nothing.

| Rate budget (per minute) | free | founding | pro | team | Category |
|---|---|---|---|---|---|
| `api_writes_per_minute` | 600 | 900 | 1200 | 1800 | POST/PATCH/DELETE on `/api/v1/*` |
| `api_reads_per_minute` | 6000 | 9000 | 12000 | 18000 | GET/HEAD/OPTIONS on `/api/v1/*` |
| `bulk_ops_per_minute` | 30 | 45 | 60 | 90 | `/api/v1/targets/bulk*` |
| `test_now_per_minute` | 60 | 90 | 120 | 180 | `POST /api/v1/targets/test` + the notification-channel test endpoints |
| `check_now_per_minute` | 60 | 90 | 120 | 180 | `POST /api/v1/targets/{id}/check-now` |

One category sits outside the plan: `support` (`POST /api/v1/support`, the
in-app help form) is capped at a fixed 2 per minute on every tier. It spends the
operator's mail budget rather than a tenant resource, so paying more does not
buy a larger share of it.

## How quotas are enforced

A resource quota is checked **atomically at the write**, not by a
check-then-act in the handler. The friendly handler-side pre-check exists
only to produce a clean error on the common, uncontended path; the race-safe
guarantee is in the store:

Each guard takes a **per-account** advisory lock, because the count it protects
spans the account's orgs: two creates in two different orgs of one account have
to contend, not race.

- **Targets** — the count bound is inside the `INSERT` (single and bulk),
  handed the same `max_targets`. Concurrent creates at `limit - 1` settle at
  exactly `limit`, never more.
- **Members** — the membership insert runs under the account lock, counts
  distinct people, and rolls itself back if it crossed `max_members`. Re-adding
  an existing member stays a no-op, and so does adding someone who already
  holds a seat in a sibling org.
- **Pending invitations** — dedupe (per org) and the pending cap (per account)
  are enforced in one transaction under the account lock; parallel
  duplicate-email invites yield exactly one row.
- **Public components** — the cap is enforced when a monitor is added as a
  status-page component, counting distinct monitors across all of the account's
  pages in the same transaction as the insert.
- **Organizations** — `create` counts the account's live orgs under its lock
  and refuses past `max_orgs` with 422 `OWNER_ORG_LIMIT`; restore re-checks the
  same number.
- **API tokens** — count-in-`INSERT`, scoped per user, handed
  `max_api_tokens_per_user`. This one is genuinely per user, not per account.

Exceeding a resource quota returns **422**:

```jsonc
{
  "error": {
    "code": "QUOTA_EXCEEDED",
    "message": "max_targets limit reached: 20 of 20 used on the free plan.",
    "field": null,
    "details": { "quota": "max_targets", "current": 20, "limit": 20, "plan": "free" },
    "trace_id": null
  }
}
```

The pending-invitation cap is the one exception to the code: it predates the
unified envelope and returns **409 `INVITATIONS_LIMIT`**. The cap itself is
enforced identically (atomic, never overshoot).

A sub-minimum check interval is its own 422, `MIN_CHECK_INTERVAL`, enforced
on create and PATCH, single and bulk — a target created at the floor cannot
be edited below it. The floor is `max(plan.min_check_interval_secs, kind_min)`:
the per-kind value (43200 for `domain_expiry`, 3600 for `tls_cert`, 300 for
`flow`, 60 for `heartbeat`, 10 for the rest) applies regardless of plan tier —
polling an expiry probe faster yields no signal, and `domain_expiry` reads
RDAP, which rate-limits by source address.

The write is not the last word. A plan can move under a monitor that already
exists, and refusing the next edit would not slow a monitor already running
three times faster than its tier allows. So the floor is applied again where
work is handed out: the scheduler's own set and the answer an agent pulls are
both clamped to `max(stored interval, plan floor)` on every refresh. The region
cap is applied the same way: a region is served a monitor only while it sits
within the first `max_regions` of that monitor's enabled regions, default-on
ones first and then by id, so every node drops the same regions. No-data
detection reads the same set, so a live agent in a region the plan has dropped
does not count as coverage and a monitor left with only dark regions is
reported unmonitored.

The clamp never writes back. The row keeps the interval its owner asked for,
so a plan that goes back up restores the original behaviour with nothing to
repair, and the usage page keeps showing what was requested. A plan change is
visible within `plan_cache_ttl_secs`; the agent pull's etag digests each org's
plan, floor, region cap and override, and is counted over the monitors the cap
actually admits, so neither a tier change nor a region enabled or disabled
elsewhere can hide behind a `304`. If a plan lookup fails, those monitors run
as stored and a warning is logged: one unreadable org must not stop everyone's
monitoring.

Feature flags are gated at the write, like the counted caps. Creating an SMS
channel on a plan without `sms_alerts_enabled` returns **403
`SMS_ALERTS_DISABLED`**, naming the feature rather than a limit of zero. The
same answer comes back from a test send of an unsaved SMS config, which is the
create path's rehearsal. A channel created before the plan moved keeps working:
it still sends, it still tests through its own endpoint, and its config stays
editable, so a number can be corrected and a leaked token rotated. Only a
self-hosted install is exempt, where the `plans` row is the operator's own.

## Rate limiting

Two app-side tiers, both keyed on the **authenticated subject** (never the
TCP peer): `(account, category)` and `(user, category)`. Both are checked; the
account tier fires first because it protects shared resources. The per-minute
budget comes from the account's plan, except for `support`, which is fixed.
The first tier keys on the account for the same reason the resource caps pool:
one budget however many workspaces the customer splits their traffic across. The
request category is derived from the path and method:

- path contains `/bulk` → `bulk_ops`
- path ends `/test` → `test_now`
- path ends `/check-now` → `check_now`
- path ends `/support` → `support`
- any path under `/mcp` → `api_reads`, whatever the method (the JSON-RPC body hides the tool name from the middleware; probe-spawning and write tools re-check the stricter category inside the tool)
- otherwise `GET`/`HEAD`/`OPTIONS` → `api_reads`, else → `api_writes`

Exceeding a budget returns **429** with a `Retry-After` header:

```jsonc
{
  "error": {
    "code": "RATE_LIMITED",
    "message": "Too many requests.",
    "field": null,
    "details": { "scope": "per_account_api_writes", "retry_after_secs": 30 },
    "trace_id": null
  }
}
```

The limiter is a `governor` cell per `(scope, category)` key in a `DashMap`.
A janitor evicts entries idle past the threshold so the map stays bounded by
the number of *active* tenants, not by request volume; its lifetime is bound
to the limiter so a refactor cannot silently drop the sweep and leak the
map. Unauthenticated requests fall through untouched — per-IP limiting for
those (auth endpoints, org creation, the public status surface) is the
reverse proxy's job; see [Deployment](deployment.md).

Checks themselves are **not** rate-limited — the scheduler path never enters
this middleware, so monitoring throughput is unaffected.

Every quota / rate-limit / abuse rejection is recorded to the append-only
`quota_events` table (`event`, `quota_name`, `details`, hashed IP) as
fire-and-forget — it never blocks the response. It is the data source for
abuse review.

## Usage transparency

| Endpoint | Returns |
|---|---|
| `GET /api/v1/orgs/{id}/usage` | Plan + current vs limit for every pooled quota, policy values, rate budgets, feature flags. The counts are the account's totals across its live orgs, so they can exceed what the org in the path holds. Member-gated (a non-member gets the same 404 as `GET /orgs/{id}`). |
| `GET /api/v1/me/usage` | The caller's `api_tokens` (genuinely per user) and `owned_orgs` (the account's orgs against `max_orgs`), each current/limit. |

The operator UI surfaces the same numbers at `/settings/usage` as progress
bars (an unlimited self-host limit renders as ∞). Reported limit == enforced
limit by construction: both read the same plan and the same count query.

## Anti-abuse

Two deny-lists, applied when a target is created, bulk-created, updated, or
test-run. A block is a **400**, audited to `quota_events` with
`event = abuse_blocked`.

- **URL patterns** — a case-insensitive regex set of attack-recon paths
  (exposed VCS dirs, `.env`, credential paths, admin panels, WordPress
  `xmlrpc` pingback, Spring actuator, backup/dump extensions, …). A match
  is `400 URL_PATTERN_BLOCKED` / `ABUSE_BLOCKED`. The shipped patterns and
  the compiled fallback are kept byte-identical by a drift guard.
- **Domains** — a YAML deny-list (`config/abuse_denylist.yaml`) matched
  **hierarchically**: listing `example.com` also blocks
  `eu.status.example.com`. It carries the operator's own domain (don't
  monitor yourself) and competing uptime/status providers (monitoring
  another monitor forms a load-amplification chain). A match is
  `400 DOMAIN_DENYLISTED`. Dedicated monitoring SaaS are listed at the apex;
  multi-tenant status-page hosts are listed narrowly so legitimate
  vendor-status checks are not over-blocked.

The lists load at startup. With `abuse.hot_reload_enabled` set, sending
SIGHUP re-reads and validates them and swaps them in atomically; a bad edit
keeps the old rules. Without it, changes need a restart. A bad regex or
malformed YAML at startup is a clean *config error*, never a crash loop.

## Configuration

```toml
[quotas]
plan_cache_ttl_secs  = 300   # org→account→plan cache; a plans-table edit
usage_cache_ttl_secs = 10    #   takes effect within this window
default_plan         = "team"  # plan the boot-seeded owner account is placed on
```

A plans-table change is invisible until the plan cache's TTL elapses (a
cache hit is zero DB round-trips on the hot path), then the next lookup
refetches.

`free` is priced for a shared platform: its ceilings bound what one tenant
can cost the host. On your own hardware there is no such cost, so a
self-hosted install needs no setup here — `default_plan` already defaults to
`team`, giving the seeded owner account 150 monitors, ten orgs to spend them
across, a 30s check floor and 13-month retention.

Only boot-time seeding reads it, so it applies to the account behind the owner
org that first run creates and to nothing else. The operator CLI
(`bootstrap-owner`) does not use it, and an account you re-plan later is never
moved back on the next boot. A second org that owner creates joins the same
account and shares its plan and pool; a *different* person signing up opens
their own account, which gets `founding` until that tier's cutoff and `free`
after it. Set `default_plan = "free"` to opt out entirely.

The plan id is resolved against the `plans` table before the seed writes
anything, so a typo fails the boot with the name quoted and leaves nothing
half-seeded behind.

Quota *values* still live only in Postgres — `default_plan` chooses a plan,
it does not override any number in one. Raise limits with a `plan_overrides`
row (keyed by account, `max_orgs` included) naming the cap fields you want
raised. Edit a shipped `plans` row only if you are prepared to reapply the
change: the catalog owns those rows and an upgrade can rewrite them.

## When a plan no longer covers what an account has

A plan can move under an account that is already using more than it sells. The
excess is never deleted. What does not fit is **held**: the row keeps every
setting it has and stops being served, and the plan growing back releases it
untouched.

| Resource | Held effect |
|---|---|
| Monitors | Not probed, no alerting, drops off public pages and share links |
| Status pages | 404 publicly, no subscriber mail |

`enabled` is the customer's own switch and is never written, so a monitor that
was paused before the hold comes back paused. Held rows still count against the
cap: the slot is what they are waiting for, and excluding them would let an
account create fresh monitors on top of the ones it has parked.

By default the oldest rows keep the slots and the newest are held, on the
grounds that the oldest are what the account was built around. `PUT
/api/v1/account/holds` replaces that guess with `keep_monitors` and
`keep_status_pages`, a list of ids each; `GET` on the same path lists what is
currently held.

A list is authoritative both ways: what it names runs, and what it leaves out
is held even when a slot is free. Refilling a spare slot from the rows the
customer just declined would make un-picking one do nothing, since the row that
lost the slot is the one that ranks first to take it back — so keeping fewer
than the plan sells is theirs to choose. An empty list is not "hold
everything", it is a reset back to the default order, and an *omitted* list
leaves that resource's previous answer alone, so a caller shown only its
monitors cannot wipe the status page choice. `{}` is therefore a plain
reconcile.

The pick only binds while a cap is exceeded. Once the plan covers the whole
pool everything is released, including rows the pick left out, because a hold
is the plan's mechanism and `enabled` is how a customer quiets a monitor inside
their plan; the pick itself is dropped at the same moment, so it cannot arm a
later shortage it was never asked about. Each cap is judged on its own: running
more browser flows than the plan covers holds flows, and says nothing about
ordinary monitors that are inside their own cap.

Both endpoints check the caller against `accounts.owner_user_id`, and so does
the picker on **Settings → Usage**. The pool spans every organization the
account owns, so an ordinary member of one of them is shown how much is held
but not the rows, which would name a sibling organization's monitors. Every hold and release
writes an org audit row (`target.plan_hold` / `target.plan_release`, and the
`status_page.` pair).

Reconciliation runs when a monitor or page is deleted, when the customer picks,
at startup, and once a day for every account that is over a cap or holding
something. Those last two are what notice a plan changed by an operator
`UPDATE`, which notifies nothing on its own.

On-call and escalation are gated separately and at write time only, like text
message alerts: a plan without `on_call_enabled` refuses a new policy, schedule,
override or contact wiring with `403 ON_CALL_DISABLED`, while everything already
built keeps paging and stays editable. Clearing a binding is always allowed. A
self-hosted install is exempt, since the operator owns the `plans` row.

Every numeric quota / rate / interval is validated at config load —
`< 1` is rejected with the offending field named, never a panic in
router or limiter construction.

The reverse-proxy per-IP tiers (auth endpoints, org creation, public
surface) are documented in [Deployment](deployment.md).
