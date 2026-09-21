# Configuration

Defaults live in `config/default.toml`. Every key can be overridden by an environment variable using the prefix `UPTIMEPAGE_` and `__` as the nested separator.

Example: `UPTIMEPAGE_SERVER__API_BIND=0.0.0.0:8080`

Override `UPTIMEPAGE_CONFIG_PATH` to point at an alternate base config file.

## Sections

| Section | Key | Purpose |
|---------|-----|---------|
| `server` | `api_bind`, `metrics_bind` | bind addresses for REST API and Prometheus exporter |
| `runtime` | `worker_threads`, `max_blocking_threads` | Tokio runtime sizing (`0` = `num_cpus`) |
| `checker` | `max_concurrent_checks` | global concurrency cap enforced by worker pool semaphore |
| `checker` | `default_timeout_ms`, `connect_timeout_ms` | client-side timeouts applied to outbound checks |
| `checker` | `default_check_interval_secs` | fallback interval when target spec omits it |
| `checker` | `per_host_max_inflight`, `rdap_max_inflight` | per-(org, host, port) and per-TLD RDAP concurrency caps. `per_host_max_inflight` is fail-fast: an over-cap tick is dropped, no `CheckResult` written. `rdap_max_inflight` queues the lookup for a free slot, bounded by the check timeout |
| `http_client` | `tcp_keepalive_secs`, `user_agent` | per-check connection keep-alive (one request's lifetime — checks connect fresh, no pool) and the outbound `User-Agent`, which defaults to the crate version and only needs setting to override it |
| `dns` | `cache_size`, `positive_ttl_secs`, `negative_ttl_secs`, `servers` | hickory resolver — point at internal resolvers when needed |
| `security` | `allow_private_targets` | SSRF guard: when `false` (default) any target resolving to loopback / private / link-local / reserved IPs is rejected |
| `security` | `credentials_kek_base64` | 32-byte base64 key encrypting `basic_auth` / `bearer_token` at rest. Empty (default) stores plaintext — dev only |
| `security` | `trusted_proxies` | CIDR ranges whose `X-Forwarded-For` is honoured for client-IP extraction. Empty means no reverse proxy: the TCP peer is trusted as-is |
| `flow` | `enabled`, `lightpanda_path`, `max_concurrency`, `mem_limit_mb`, `block_private_networks`, `block_cidrs`, `v8_max_heap_mb`, `max_response_mb`, `user_agent_suffix` | Browser-driven flow monitors (Lightpanda engine). Off by default. See [Browser flow monitors](#browser-flow-monitors) below |
| `rate_limits` | `per_ip.*`, `janitor.*` | Mirrors of the per-IP numbers the reverse proxy enforces, plus the in-process limiter-map janitor cadence. Per-org/per-user limits come from the plans table |
| `abuse` | `url_patterns_denied`, `domain_denylist_path`, `reputation_source_path`, `hot_reload_enabled` | Deny-list of attack-recon URL patterns and domains checked at target creation. `hot_reload_enabled` lets SIGHUP swap the rules in without a restart |
| `email_policy` | `enabled`, `sources`, `refresh_interval_hours`, `signup_policy`, `require_mx`, `min_domains`, `max_domains`, `max_shrink_pct` | Disposable-address admission control. See [Disposable email addresses](#disposable-email-addresses) below |
| `circuit_breaker` | `failure_threshold`, `success_threshold`, `open_duration_secs`, `half_open_max_calls` | per-host breaker state machine |
| `storage` | `allow_default_credentials` | the shipped `monitor` database credentials are published in a public repo, so boot refuses them. Off by default; the local dev stacks turn it on. Set your own `storage.postgres.url` and `storage.clickhouse.password` anywhere else |
| `storage.postgres` | `url`, `max_connections`, `min_connections`, `acquire_timeout_secs` | target metadata store |
| `storage.clickhouse` | `url`, `database`, `user`, `password`, `batch_size`, `batch_timeout_ms`, `buffer_size` | result sink and pipeline back-pressure |
| `storage.clickhouse` | `async_insert` | coalesces the batcher's inserts into larger parts server-side. On by default; `wait_for_async_insert` stays on so the retry/dedup durability ack still holds |
| `scheduler` | `enabled` | off = this process probes nothing in-process (pure dashboard/brain); on = the in-process scheduler probes `region`. Defaults to `true` |
| `scheduler` | `target_refresh_interval_secs` | how often the registry is reconciled against Postgres |
| `scheduler` | `region`, `default_region` | this control plane's own region id (a normal region row, default `"default"`) and the region new targets are assigned to (empty falls back to `region`). See [Multi-region probes](multi-region.md) |
| `agent` | `enabled`, `control_plane_url`, `region`, `pull_interval_secs`, `flush_interval_secs`, `buffer_capacity` | run this process as a stateless regional probe instead of a control plane. `token` is **env-only** (`UPTIMEPAGE_AGENT__TOKEN`). Off by default. See [Multi-region probes](multi-region.md) |
| `operator` | `admin_token` | static bearer secret for the instance-admin `/operator/*` surface (regions + agents). **Env-only** (`UPTIMEPAGE_OPERATOR__ADMIN_TOKEN`); empty disables the surface (404s) |
| `operator` | `agent_stale_after_secs` | how long since an agent's last check-in before it's shown stale on the `/operator/*` surface |
| `observability` | `log_level`, `log_format` | tracing-subscriber filter + JSON vs pretty output |
| `observability` | `metrics_enabled`, `gauge_sample_interval_ms` | Prometheus exporter toggle and sampler cadence |
| `observability` | `tracing_enabled` | Master on/off for OTLP trace export. Export is active only when this **and** `observability.grafana.enabled` are true |
| `observability.grafana` | `enabled`, `otlp_endpoint`, `instance_id`, `api_key`, `trace_sample_ratio` | OTLP/HTTP trace export to Grafana Cloud / any OTLP collector. `api_key` is env-only. See [Trace export](#trace-export) below |
| `observability.heartbeat` | `enabled`, `url`, `interval_seconds` | external dead-man's-switch: pings `url` on an interval while every critical dependency is reachable, so an independent watcher can alert when the pings stop. `url` is env-only |
| `api.cors` | `enabled`, `allowed_origins`, `allowed_methods`, `allow_any_origin` | browser CORS for `/api/v1/*`. Disabled by default. Wildcard only via `allow_any_origin = true` |
| `api` | — | Per-IP API rate limiting is not in-process; it's enforced by the reverse proxy. In-process limiting is per-org / per-user, driven by `[rate_limits]` and the plans table |
| `escalation` | `retry_backoff_base_secs`, `retry_backoff_cap_secs`, `reconcile_window_secs` | exponential backoff for re-paging a failed channel: attempt n waits `base * 2^(n-1)`, capped. `reconcile_window_secs` bounds how far back the sweep looks for an incident whose open signal was dropped |
| _notification channels_ | — | Not a config block. Channels are **per-org runtime resources** managed via the [`/api/v1/notification-channels` API](api.md#notification-channels); secrets are sealed at rest with the credentials KEK |
| `tenancy` | `path_based_public_routes`, `subdomain_public_routes`, `deletion_grace_period_days` | Public-status routing shape + the deletion grace window. How many orgs an account may hold is a plan column (`plans.max_orgs`), not a deployment knob. See [Public status routing](#public-status-routing) below and [docs/multi-tenancy.md](multi-tenancy.md) for the full model |
| `retention` | `login_attempts_days`, `quota_events_days`, `audit_log_days`, `mcp_audit_days`, `api_tokens_post_expiry_days` | Long-horizon data-retention windows for the daily 03:00-UTC purge job. Every key is bound by the job — no decorative knobs |
| `public_status` | `base_domain`, `cache_max_orgs`, `cache_ttl_secs`, `last_good_ttl_secs`, `max_logo_size_bytes`, `allowed_logo_mime_types`, `max_logo_dimension_px`, `default_brand_color`, `default_show_powered_by`, `public_per_ip_rate_limit_per_min` | Per-org public status pages at `{slug}.{base_domain}`. Logo bytes live in the Postgres `page_assets` table, not on disk. See [Public status page](#public-status-page) below and [Per-org status pages](per-org-status.md) |
| `auth` | `enabled_methods`, `open_signup`, `fingerprint_salt`, `public_base_url` | Sign-in methods, HMAC salt for IP/UA hashes, base URL embedded in invitation + magic-link emails. `open_signup` (default `true`) lets an uninvited address open an account with an org of its own; set it `false` for an invite-only deployment, where existing users still sign in and invitations still work. Needs `magic_link` in `enabled_methods`. See [Auth configuration](#auth-configuration) below |
| `auth.session` | `idle_timeout_days`, `absolute_timeout_days`, `cookie_name`, `cookie_secure`, `cookie_domain`, `renew_on_use` | Session cookie shape + lifetime. `cookie_secure = true` in production |
| `auth.github` | `client_id`, `client_secret`, `redirect_url`, `scopes` | GitHub OAuth client. The button renders on `/login` only when client_id, client_secret, and redirect_url are all set |
| `auth.google` | `client_id`, `client_secret`, `redirect_url`, `scopes` | Google OAuth client, same gating as `auth.github`. Email is trusted only with Google's `email_verified` attestation |
| `auth.microsoft` | `client_id`, `client_secret`, `redirect_url`, `scopes`, `tenant` | Microsoft (Entra ID + personal accounts) OAuth client, same gating as `auth.github`. `tenant` picks which accounts may sign in: `common`, `organizations`, `consumers`, or one tenant GUID / domain — an unaddressable value fails the boot rather than falling back to `common`. Email is trusted only with the `xms_edov` optional claim, or on a Microsoft-owned domain in the personal tenant |
| `auth.gitlab` | `client_id`, `client_secret`, `redirect_url`, `scopes`, `base_url` | GitLab OAuth client, same gating as `auth.github`. `base_url` names the instance the application is registered on — gitlab.com or a self-managed https origin; a non-https or malformed value fails the boot. It is half the identity key (`{iss}/{sub}`), so changing it after sign-ups orphans those accounts. Email is trusted only with GitLab's `email_verified` claim |
| `auth` passkeys | none | Passkeys have no client credentials to configure: they answer to this deployment alone. Enabled by `enabled_methods` including `"passkey"`, plus an `auth.public_base_url` with a host, whose host becomes the relying-party id. WebAuthn needs a secure context, so they are unavailable over plain http on a real hostname. **Changing that host retires every passkey already created**; nothing can migrate them, and boot logs a warning counting the casualties. At most 10 per account |
| `auth.api_tokens` | `prefix_visible_chars` | Indexed prefix length for token lookup. The per-user token cap is a plan quota (`plans.max_api_tokens_per_user`), not a config key |
| `auth.invitations` | `expiry_hours` | Invitation lifetime. The per-org pending cap is a plan quota (`plans.max_pending_invitations`), not a config key |
| `auth.magic_link` | `expiry_minutes`, `rate_limit_seconds` | Lifetime of both credentials in the mail, the link and the six-character code beside it. The code is bound to the browser that asked for it and gets one attempt; a miss leaves the link redeemable. Sending a new mail retires every earlier link and code for that address. Routes only mount when `enabled_methods` includes `"magic_link"` |
| `bootstrap` | `email`, `org_name` | Seeds the first owner at boot when the instance has no users, for installs with no terminal to run `bootstrap-owner` in. Empty `email` (default) disables it. See [First-run owner](#first-run-owner) below |
| `mcp` | `enabled`, `oauth_enabled`, `resource_uri`, `allowed_origins`, `access_token_ttl_secs` | LLM connector (MCP) server at `/mcp`. Off by default; OAuth requires real HTTPS `resource_uri` + `auth.public_base_url`. See [MCP server](mcp.md) |
| `email` | `provider`, `from_name`, `from_address`, `support_address` | Every mail the product sends. `provider` ∈ `"resend" \| "log" \| "memory"`. `from_address` is required by `resend` and should be an inbox somebody reads, not `no-reply@`. A set `support_address` mounts the in-app help form at `/help` and relays it there, with the sender as `Reply-To`; empty (default) leaves the page, its endpoint and its nav entry absent |
| `email.resend` | `api_key`, `webhook_secret` | `api_key` required when `email.provider = "resend"`. A set `webhook_secret` (the endpoint's Svix `whsec_…` signing secret) mounts `POST /hooks/resend`: a permanently bounced or spam-complaining address gets every email channel pointed at it disabled, with the reason shown on the channel form |
| `whatsapp_app` | `enabled`, `access_token`, `phone_number_id`, `public_number`, `app_secret`, `verify_token`, `template_name`, `language_code` | Operator WhatsApp number behind one-tap `whatsapp_app` channels (`wa.me` deep link + `/hooks/whatsapp` Meta webhook). `enabled = true` AND complete creds mount the surface — the flag is a deliberate spend gate, since alert sends are operator-paid Meta template messages. Inbound `stop` disables the sender's channels |

## Public status routing

uptimepage ships from one binary as a multi-tenant SaaS. The active org is always resolved from the authenticated session; there is no ambient "default org" and no compile-time self-host mode. A single-tenant deployment is just a SaaS deployment where you sign up as the first user (or seed `users` + `organizations` + `memberships` via a SQL one-shot).

The public status surface is gated by **two** independent flags because path-based and subdomain routing have opposite safety profiles:

- `tenancy.path_based_public_routes` — serve `/status` and `/api/public/v1/*` on the operator host, scoped to the single live org. Useful for a single-tenant deploy (one org, one page). Defaults to `true`. Safe by construction once you have more than one tenant: the lookup only resolves when exactly one live org exists, so a second org doesn't leak the first org's data, it 404s the path-based surface for everyone. Turn it off once you're on subdomain routing so the operator host doesn't 404 on every visit.
- `tenancy.subdomain_public_routes` — serve one page per org at `{slug}.{public_status.base_domain}` (apex wildcard). Defaults to `false`; requires a well-formed `base_domain`.

| Shape | Recommended flags | Public surface |
|---|---|---|
| Single-tenant | `path_based_public_routes = true` (default) | `/status` on the operator host (one org) |
| Multi-tenant SaaS | `subdomain_public_routes = true`, `path_based_public_routes = false` | `{slug}.{base_domain}` per org |

The binary refuses to boot in the dangerous combinations: `subdomain_public_routes` with an empty or single-label `public_status.base_domain`; or an `auth.session.cookie_domain` that overlaps the status wildcard. Each is a loud panic at startup, not a silent runtime leak. See [Per-org status pages](per-org-status.md) for the full model.

### Org limits and the purge worker

- How many orgs one account may hold comes from its plan's `max_orgs` column (free 1, founding 3, pro 5, team 10), counted under a per-account lock so concurrent creates can't exceed it. Soft-deleted orgs don't count.

- `deletion_grace_period_days` (default `30`) is how long a soft-deleted org's slug is held and how long the original deleter has to restore it.
- The soft-delete purge now runs inside the daily retention job (`src/jobs/retention.rs`) at a fixed 03:00 UTC, not on a configurable interval. Each run cascades up to 10 past-grace orgs, drains any pending entries from `clickhouse_purge_queue` (the outbox between PG cascade and ClickHouse `ALTER TABLE DELETE`), hard-purges past-grace users, then enforces the `[retention]` windows. See [Soft delete and the 30-day purge](multi-tenancy.md#soft-delete-and-the-30-day-purge) for the full implementation and failure-recovery guarantees.

The `[retention]` section sets the long-horizon windows. Defaults: `login_attempts_days = 180`, `quota_events_days = 90`, `audit_log_days = 730`, `mcp_audit_days = 730`, `api_tokens_post_expiry_days = 30`. Check-result retention is **not** a config knob. Each `check_results` row carries its own `ttl_days` (ClickHouse `UInt16 DEFAULT 30`), stamped at write time from the writing org's plan `raw_days`; every seeded plan currently sets `raw_days = 30`, so raw per-check rows keep 30 days. The hourly rollup `check_results_1h` keeps a fixed 13 months, set at migration time. A plan's `raw_days` change takes effect on the next write, not retroactively, since the TTL is stamped per row rather than re-issued as an `ALTER` on boot. The public status page's daily history strip still shows 90 days, drawn from the rollup table, not the raw one. Session idle/absolute reaping uses `[auth.session]`; soft-deleted org/user grace uses `tenancy.deletion_grace_period_days`; OAuth-state and magic-link tokens are swept by their own short-cadence jobs.

See [Multi-tenancy](multi-tenancy.md) for the full model, slug rules, and the storage-layer isolation invariants the CI checks enforce.

## Auth configuration

```toml
[auth]
enabled_methods = ["github_oauth", "google_oauth", "microsoft_oauth", "gitlab_oauth", "magic_link"]
fingerprint_salt = ""                # HMAC salt for IP/UA hashes; rotate-aware
public_base_url = "https://status.example.test"

[auth.session]
idle_timeout_days = 30
absolute_timeout_days = 90
cookie_name = "_sm_session"
cookie_secure = true                 # set false only for plain-HTTP local dev
cookie_domain = ""                   # empty = host-only cookie
renew_on_use = true

[auth.github]
client_id = ""                       # from https://github.com/settings/developers
client_secret = ""
redirect_url = "https://status.example.test/auth/github/callback"
scopes = ["user:email", "read:user"]

[auth.google]
client_id = ""                       # Google Cloud Console OAuth web client
client_secret = ""
redirect_url = "https://status.example.test/auth/google/callback"
scopes = ["openid", "email", "profile"]

[auth.microsoft]
client_id = ""                       # Entra app registration (portal.azure.com)
client_secret = ""
redirect_url = "https://status.example.test/auth/microsoft/callback"
scopes = ["openid", "email", "profile"]
tenant = "common"                    # common | organizations | consumers | <tenant GUID or domain>

[auth.gitlab]
client_id = ""                       # GitLab application (User settings -> Applications)
client_secret = ""
redirect_url = "https://status.example.test/auth/gitlab/callback"
scopes = ["openid", "email", "profile"]
base_url = "https://gitlab.com"      # or a self-managed instance's https origin

[auth.invitations]
expiry_hours = 168                   # 7 days; pending-invite cap is plans.max_pending_invitations

[auth.api_tokens]
prefix_visible_chars = 16            # floor; lower values fail boot; per-user cap is plans.max_api_tokens_per_user

[auth.magic_link]
expiry_minutes = 15                  # covers the link and the code printed beside it
rate_limit_seconds = 60                # per-email send throttle; 0 disables

[email]
provider = "log"                     # "resend" in prod, "log" in dev, "memory" in tests
from_name = "Uptimepage"
from_address = "hello@example.test"    # a read inbox, not no-reply
support_address = ""                 # set it and /help appears; empty = no help form

[email.resend]
api_key = ""                         # required when provider = "resend"
webhook_secret = ""                  # whsec_… of the Resend webhook endpoint

[billing]
provider = "none"                    # "paddle" turns on checkout + the webhook receiver; "none" leaves it all absent

[billing.paddle]
environment = "sandbox"              # "sandbox" | "live" — keys are bound to one
api_key = ""                         # env UPTIMEPAGE_BILLING__PADDLE__API_KEY
webhook_secret = ""                  # env …__WEBHOOK_SECRET — the destination's pdl_ntfset_… key
client_token = ""                    # Paddle.js client-side token; public, embedded on /pay

[whatsapp_app]                       # operator WhatsApp number (one-tap linking)
enabled = false                      # deliberate spend gate — creds alone stay off
access_token = ""                    # Meta Cloud API token (env-only)
phone_number_id = ""                 # Cloud API sender id
public_number = ""                   # display number digits — the wa.me target
app_secret = ""                      # signs webhook deliveries (env-only)
verify_token = ""                    # echoed by Meta's GET subscribe handshake
template_name = ""                   # approved alert template, single body param
language_code = "en"
```

`auth.enabled_methods` is the policy switch per sign-in method: removing
an entry disables that method's login start/callback (404) and hides its
button. OAuth providers additionally need client_id + client_secret +
redirect_url set — a listed but incompletely configured provider stays
hidden and logs a warning on probe. `"magic_link"` mounts the magic-link
request, verify and code endpoints along with the login-page email form.

`auth.fingerprint_salt` is paired with the `auth_salt_history` table.
Rotating the value mid-deployment refuses to boot unless the override
env var documented in `docs/troubleshooting.md` is set — this is
deliberate so audit-trail breakage is loud.

## Disposable email addresses

Signup by emailed link or code means anyone can open an account with a
throwaway address. Two signals under `[email_policy]` decide what happens:

- a **disposable-domain corpus**, refreshed from the configured `sources` and
  held in memory, and
- an **MX check**, asking whether the domain accepts mail at all.

**Off by default.** The MX check resolves through the public servers in
`[dns] servers`, so an install whose staff mail lives on an internal domain
would get "no mail exchanger" for its own addresses and refuse them
everywhere. Set `enabled = true` when the addresses you serve are public
ones. Nothing is fetched and no lookup happens while it is off.

The default sources are two public lists — one regenerated every 24 hours
(~75k domains, MIT), one curated by pull request (~8k, CC0). Their union is
refreshed on `refresh_interval_hours`, stored in Postgres, and reloaded at
boot, so a restart never leaves a window where every address passes. Across
replicas an advisory lock means only one process fetches per interval.

### What happens where

| Surface | Behaviour |
|---|---|
| Signup (magic link, OAuth) | `signup_policy` — `flag` (default), `block`, or `allow` |
| Sign-in for an existing account | Never refused |
| Magic-link send | Skipped for a listed domain under `block` only |
| Invitation sent | Refused |
| Invitation accepted | Opened and marked, never refused |
| Status-page subscription | Refused |
| Email notification channel | Refused, unless already verified |

The split is deliberate. The surfaces that mail an address somebody handed us
always refuse, for two reasons: an address with no exchanger bounces, and
bounce rate is a sender-reputation number shared by every tenant on the
instance; and a throwaway inbox nobody reads means the alert never lands,
which is worse than no channel at all because it looks configured. Signup
defaults to `flag` instead: the account opens and carries
`users.email_risk`, visible in the back-office user list. A corpus of 75k
domains will eventually contain one it should not, and losing a paying
customer to a false positive costs more than the account it would have
stopped. Move to `block` once you have watched the flags for a while.

Under `block` the magic link is not sent to a listed domain at all, since
`/verify` would refuse it and the link could never be redeemed. Under `flag`
it is sent: the account has to open for the mark to exist. This saves a
pointless mail, not a bounce — listed throwaway domains generally do accept
mail, and it is the MX check that catches the addresses that bounce.

An address with **no MX at all** is refused everywhere regardless, including
under `flag`: an account there could never receive the alerts it exists to
send. The lookup fails open, so a resolver outage cannot close signups.

Two exemptions keep the gate from stranding people. An address a tenant has
already verified as a notification channel stays editable and testable even
if a list later names its domain — the channel is still delivering, and the
pinned floor is compile-time, so there would otherwise be no way to clear a
false positive without a release. And an account that existed before its
domain was listed keeps signing in: the corpus governs who may open an
account, not who may come back.

### Guards

The corpus is third-party data on the signup path, so an upstream edit is an
upstream write to admission control. Two independent limits bound that:

- **By size.** A fetched list below `min_domains`, above `max_domains`, or
  more than `max_shrink_pct` below the stored one is rejected and the previous
  corpus stays live. This catches a truncated or half-published upstream,
  which otherwise arrives as a perfectly well-formed short file.
- **By name.** `security::email_policy::NEVER_DISPOSABLE` pins mainstream mail
  providers, privacy relays (SimpleLogin, addy.io, Firefox Relay, DuckDuckGo,
  Apple Hide My Email), and multi-label public suffixes such as `co.uk`. No
  upstream entry can reach any of them. This is not hypothetical: the stricter
  upstream variants already list several relay domains, and relays are
  permanent forwarding addresses people pay for, not burners.

## First-run owner

A fresh instance has no accounts, and every sign-in method assumes one already exists. The normal way to create it is the CLI:

```bash
uptimepage bootstrap-owner --email you@example.com
```

That prints a full-access API token, so it needs a terminal. App-store and appliance installs do not have one. For those, name the owner in config instead:

```toml
[bootstrap]
email = "you@example.com"
org_name = "Home Lab"
```

```bash
UPTIMEPAGE_BOOTSTRAP__EMAIL=you@example.com
UPTIMEPAGE_BOOTSTRAP__ORG_NAME='Home Lab'
```

On boot, if the `users` table is empty, this creates the owner and its org and logs a sign-in link:

```
WARN seeded the first owner; open sign_in_url to claim this instance
  org=round-hill-r5m8md
  sign_in_url=https://status.example.test/auth/magic-link/verify?token=…
  expires_in_hours=24
```

Open the link to claim the instance. Notes on the shape of this:

- It runs once. Any user row, soft-deleted included, means the instance is already claimed, and boot skips the whole block. The setting is inert from then on, so it is safe to leave in place: a value that later goes stale or malformed cannot break a restart.
- On an unclaimed instance a malformed `bootstrap.email` fails boot rather than being ignored, and so does a missing `"magic_link"` in `auth.enabled_methods`, since there would be no way to hand the link back. Failing loudly beats seeding an account nobody can reach.
- Only a single-use sign-in link is emitted, never an API token. In a container stdout **is** the log stream, and a token there would be a long-lived credential sitting in log storage. Mint tokens from the UI after signing in.
- The link lives 24 hours rather than the usual `auth.magic_link.expiry_minutes`, because whoever installed the app may not read the logs for a while.
- The link is minted before the account, so a failure partway through leaves no user row and the next boot retries cleanly.
- The seeded email is not logged. It is already in your own config, and repeating it into log storage only spreads it further.

Over plain HTTP on a LAN, also set `auth.session.cookie_secure = false`; the default `true` means the browser drops the session cookie the link issues.

Deleting the seeded owner and letting the purge job clear it past its grace period empties `users` again, and the next restart will seed it afresh from the same config. Clear `bootstrap.email` once the instance is claimed if that is not what you want.

## Payments

`billing.provider = "paddle"` turns on the paid subscription lifecycle: checkout, the webhook receiver at `/hooks/billing/paddle`, the pay page at `/pay`, and the `/api/v1/account/billing` surface. `"none"` (the default) leaves every one of them absent — a self-host install never needs it, since a self-hoster owns the `plans` row directly.

Paddle is the merchant of record, so tax, invoicing and card retries are Paddle's; the app only mirrors what Paddle decides onto the account's plan. Boot refuses a half-configured provider: `paddle` needs an api key, the notification destination's webhook secret, a client token, a valid `environment`, and an `https://` `auth.public_base_url` (the webhook and pay page live on it). All three secrets are env-only in production (`UPTIMEPAGE_BILLING__PADDLE__API_KEY`, `…__WEBHOOK_SECRET`); the client token is public by design.

Which price sells which plan is data, not config: rows in `plan_prices` (`provider`, `price_ref`, `plan_id`, `interval`, `amount_minor`, `currency`) map a Paddle price id to a catalog plan, with the amount as Paddle quotes it so the billing page can show it without a call. Adding a price or a whole second provider is an INSERT, and only a listed plan (`plans.is_listed`) is offered:

```sql
INSERT INTO plan_prices (provider, price_ref, plan_id, interval, amount_minor, currency) VALUES
  ('paddle', 'pri_01abc…', 'pro', 'month',  900, 'USD'),
  ('paddle', 'pri_01def…', 'pro', 'year',  9000, 'USD');
```

A plan with only one cadence is sold on that cadence alone; the billing page offers the yearly toggle once any plan carries a `year` row. The account id is attached to the Paddle transaction as `custom_data.account_id`, next to an HMAC tag keyed by a secret the server generates on first boot and keeps in its database, and travels back on every event. A webhook honours the id only under that tag, since Paddle.js lets any visitor open a checkout with custom data of their own; a subscription already bound to an account answers to that account whatever the event names, so the tag only ever decides a first binding.

## Central Telegram bot

```toml
[telegram]
bot_token = ""            # env UPTIMEPAGE_TELEGRAM__BOT_TOKEN; presence enables the feature
bot_username = ""         # verified against the Bot API at boot; used for t.me deep links
webhook_secret = ""       # random, 32+ chars; Telegram echoes it on every webhook delivery
```

Setting `bot_token` switches on one-tap Telegram channel linking: the
type card in the channel form, the link-code API, and the
`/hooks/telegram` receiver. Empty token (the default) leaves the
feature absent entirely — self-host deployments keep the
bring-your-own `telegram` transport, which needs no operator config.

When enabled, boot validates the trio: non-empty `bot_username`,
`webhook_secret` of 32+ characters, and an `https://`
`auth.public_base_url` (Telegram only delivers webhooks to public
https endpoints). The app then verifies the token against the Bot API
and registers the webhook on every boot; a Telegram outage logs a
warning and disables the bot for that boot instead of failing the
deploy.

All three values are operator secrets: env-only in production, never
in a committed config file.

## Provider OAuth connect ("Add to Slack" / "Add to Discord")

```toml
[slack_oauth]
client_id = ""            # env UPTIMEPAGE_SLACK_OAUTH__CLIENT_ID
client_secret = ""        # env UPTIMEPAGE_SLACK_OAUTH__CLIENT_SECRET

[discord_oauth]
client_id = ""            # env UPTIMEPAGE_DISCORD_OAUTH__CLIENT_ID
client_secret = ""        # env UPTIMEPAGE_DISCORD_OAUTH__CLIENT_SECRET
```

Credentials of operator-owned OAuth apps — Slack with the
`incoming-webhook` scope, Discord with `webhook.incoming`. When a pair is
set, that provider's panel in the channel form grows a connect button
(plus a QR variant): the provider's consent screen picks the destination
channel and the callback stores the returned webhook as a regular
`slack`/`discord` channel — access tokens are discarded. The app's
redirect URL must be `<auth.public_base_url>/auth/slack/callback` (or
`…/auth/discord/callback`). Empty credentials (the default) hide the
button; manual webhook paste always works. Env-only in production, never
in a committed config file.

## Public status page

The `[public_status]` block configures the per-org public surface. It is
load-bearing only when `tenancy.subdomain_public_routes = true`; the
defaults are safe to leave untouched for self-host.

```toml
[public_status]
base_domain = ""                       # REQUIRED when subdomain_public_routes = true
cache_max_orgs = 1000                  # hot + last-good cache bound
cache_ttl_secs = 10                    # per-org rendered-page TTL
last_good_ttl_secs = 3600              # idle eviction for the stale-fallback layer
max_logo_size_bytes = 1048576          # 1 MiB byte ceiling (pre-decode)
allowed_logo_mime_types = ["image/png", "image/jpeg", "image/webp"]
max_logo_dimension_px = 1200           # larger uploads are downscaled; decode
                                       # is also allocation-bounded (bomb guard)
default_brand_color = "#3b82f6"        # used when an org sets no colour
default_show_powered_by = true
public_per_ip_rate_limit_per_min = 60  # in-app limit behind the Caddy-side one
```

| Key | Purpose |
|---|---|
| `base_domain` | parent domain for `{slug}.{base_domain}`. Must be multi-label; boot fails on empty/single-label when subdomain routing is on |
| `cache_max_orgs` / `cache_ttl_secs` | per-org page cache size and freshness window |
| `last_good_ttl_secs` | how long an idle org's last-known-good snapshot is retained before eviction |
| `max_logo_size_bytes`, `allowed_logo_mime_types`, `max_logo_dimension_px` | logo upload limits. The bytes themselves are stored in the Postgres `page_assets` table, not on disk |
| `default_brand_color`, `default_show_powered_by` | fallbacks when an org leaves branding unset |
| `public_per_ip_rate_limit_per_min` | second-layer rate limit behind the reverse proxy's |

History-strip length (90 days) and the recent-incidents horizon (30 days)
remain hard-coded defaults in `src/public_status/aggregator.rs`. What a
page publishes is curated per-page — a monitor appears as a component
only while it's bound to that page, and its presentation lives on the
binding:

| Per-page component field | Purpose |
|---|---|
| (binding exists) | the monitor is published as a component on that page |
| `public_name` | display name (falls back to operator-side monitor name) |
| `public_description` | optional one-liner |
| `public_group` | optional group label; ungrouped components render last |
| `sort_order` | ASC integer sort within a group |

See [Public status page](public-status.md) for the operator workflow and
[Per-org status pages](per-org-status.md) for the SaaS subdomain model.

## Trace export

OpenTelemetry spans are exported over OTLP/HTTP (protobuf) when **both**
`observability.tracing_enabled` and `observability.grafana.enabled` are
`true`. Disabled by default and zero-cost when off.

```toml
[observability]
tracing_enabled = false                # master on/off for trace export

[observability.grafana]
enabled = false                        # second switch; both must be true
otlp_endpoint = ""                     # OTLP base, no /v1/traces suffix; e.g.
                                       # https://otlp-gateway-<zone>.grafana.net/otlp
instance_id = ""                       # Grafana Cloud numeric instance / stack id
trace_sample_ratio = 0.05              # example override; shipped default is 1.0 (keep-all)
# api_key                              # NEVER in TOML — env var only (below)
```

| Key | Purpose |
|---|---|
| `tracing_enabled` | master switch; with `grafana.enabled` gates all export |
| `grafana.enabled` | second switch (kept separate so the block is inert until explicitly turned on) |
| `grafana.otlp_endpoint` | OTLP/HTTP **base** URL; the service appends `/v1/traces` (a value already ending in it is left as-is). Empty fails boot when export is on |
| `grafana.instance_id` | basic-auth username (Grafana Cloud instance id). Empty fails boot when export is on |
| `grafana.api_key` | basic-auth password. **Env-only**: `UPTIMEPAGE_OBSERVABILITY__GRAFANA__API_KEY`. Never read from a config file; redacted in any serialised config |
| `grafana.trace_sample_ratio` | head sampling ratio under a parent-based sampler. Must be in `[0.0, 1.0]` or boot fails |

Auth is `Authorization: Basic base64(instance_id:api_key)`. Resource
attributes `service.name = uptimepage` and `service.version` are
attached. The batch exporter is flushed and stopped on graceful
shutdown. A transport build failure logs a warning and the service
continues without traces — telemetry never takes down monitoring.
Inconsistent settings (export on with a missing endpoint / instance /
key, or an out-of-range ratio) are a clean startup config error.

## Browser flow monitors

A [flow monitor](monitor-types.md#flow) drives a real headless browser through a login, so it costs far more than any other check kind: one browser process per run, held for the length of the run. The block is off everywhere by default, and turning it on is a deliberate decision about which nodes can afford it.

```toml
[flow]
enabled = false                        # this node runs flow checks
lightpanda_path = "lightpanda"         # engine binary; absolute path in a container
max_concurrency = 2                    # simultaneous browser processes on this node
mem_limit_mb = 250                     # per-run RSS ceiling; over it the run is killed as Error
block_private_networks = true          # runtime SSRF guard, after DNS resolution
block_cidrs = "169.254.0.0/16,127.0.0.0/8,100.64.0.0/10,::1/128,fc00::/7,fe80::/10"
v8_max_heap_mb = 0                     # in-engine JS heap cap; 0 = engine default
max_response_mb = 0                    # reject any single browser response over this; 0 = no limit
user_agent_suffix = ""                 # appended to the browser UA for attribution
```

| Key | Purpose |
|---|---|
| `enabled` | Whether this process runs flow checks at all. An agent self-reports the answer, and the control plane never sends a flow to a node that said no |
| `lightpanda_path` | Where the engine binary is. The shipped images put it at `/usr/local/bin/lightpanda` |
| `max_concurrency` | How many browser processes may run at once here. Each run spawns a fresh process and tears it down after, so this is the real memory lever |
| `mem_limit_mb` | Per-run resident memory ceiling. A run that crosses it is killed and recorded as `Error`, not `Down`: the page did not fail, this node refused to keep paying for it. `0` disables the watchdog |
| `block_private_networks` | Blocks the browser's own outbound requests to private and internal addresses, after DNS resolves. The save-time URL check cannot cover this, because redirects, `fetch` calls and DNS rebinding all resolve later |
| `block_cidrs` | Extra ranges to block, comma-separated. The default list covers cloud metadata, loopback, CGNAT, and IPv6 unique-local and link-local. A `-` prefix exempts a range |
| `v8_max_heap_mb` | JS heap cap inside the engine. Set it below `mem_limit_mb` so a runaway script trips the cheap limit before the process-level one |
| `max_response_mb` | Refuses any single response larger than this, so one large asset cannot blow the memory ceiling on its own |
| `user_agent_suffix` | Appended to the browser's User-Agent, so a site owner reading their logs can tell what the traffic is |

Two switches have to line up before anyone can create one, and they are independent on purpose:

- **`flow.enabled`** decides which nodes *can run* a flow. Set it on the control plane, on selected agents, or on nothing.
- **`plans.max_flow_checks`** decides which orgs *may create* one, and how many. It is seeded at 0, so the API answers `403 FLOW_CHECKS_DISABLED` until an operator raises it. It is both the cap and the kill switch.

Enabling the engine on a node does not let anyone create a flow, and raising the plan cap does not make one run. You need both.

In the shipped compose stack the env var is `AGENT_FLOW_ENABLED`, which maps to `UPTIMEPAGE_FLOW__ENABLED`. Set it per agent, not globally: an agent on a small host will spend its memory on browsers and start missing its other checks. A flow monitor's regions are clamped to the flow-capable set when it is saved, so an agent with the engine off simply never receives one. Quorum needs two regions to decide anything, so with a single flow-capable region the monitor reports what that one region saw. See [Multi-region probes](multi-region.md).

## Tuning notes

- **`max_concurrent_checks`** caps simultaneous in-flight checks. Per-check memory is small (a tokio task plus an in-flight hyper request), so the practical ceiling is set by file descriptors and ephemeral ports rather than RAM.
- **`per_host_max_inflight`** (default `2`) is the per-tenant per-`(host, port)` in-flight cap. One tenant fanning a burst of checks at the same upstream looks like a probe; this cap keeps that fingerprint flat. Tenant-scoped — one customer's burst never starves another customer's monitor of the same host. Fail-fast: a check that would exceed the cap is dropped, not queued, no `CheckResult` is written, so it never counts as a failure and no alert fires (the upstream is fine, the back-pressure is operator-side). Counters: `uptimepage_host_throttle_waits_total{kind="host"}` (attempts) and `uptimepage_host_throttle_drops_total` (rejections).
- **`rdap_max_inflight`** (default `1`) is the process-wide per-TLD registry-lookup concurrency cap (across all tenants), covering RDAP and the WHOIS fallback. Daily check cadence + per-TLD slot means deep queues drain quickly without bursting any registry. Unlike the per-host cap, an over-cap RDAP lookup is not dropped: it waits FIFO for the slot, bounded by the per-check timeout. A lookup that runs out of time falls through to the sticky last-good cached verdict, the same path a transient probe failure takes (see below).
- **`storage.clickhouse.buffer_size`** is the mpsc capacity between worker pool and batcher. Sized for ~1 s of bursts at peak RPS. Drops increment `uptimepage_storage_dropped_results_total{reason="queue_full"}` — that metric is your back-pressure signal.
- **`storage.clickhouse.batch_size` vs `batch_timeout_ms`** trade tail latency for throughput. `1000 / 500ms` is a good starting point at ~20k rps.
- **`scheduler.enabled`** (default `true`) turns the in-process scheduler on. Off makes the process a pure dashboard/brain with no local probing, useful once regional agents cover all check traffic.
- **`dns.servers`** accepts either bare IPs (`"1.1.1.1"`) or `ip:port` form. Used as is — no system resolver fallback.
- **`security.allow_private_targets`** is the SSRF guard. Default `false` blocks:
  - Loopback (`127.0.0.0/8`, `::1`)
  - RFC1918 private (`10/8`, `172.16/12`, `192.168/16`)
  - Link-local (`169.254/16`, `fe80::/10`) — covers AWS/GCP metadata `169.254.169.254`
  - Carrier-grade NAT (`100.64/10`)
  - IPv6 ULA (`fc00::/7`), discard, IPv4-mapped private, documentation ranges
  - Multicast, broadcast, unspecified, reserved-for-future-use
  - IPv6 transition mechanisms: `2002::/16` (6to4) and `64:ff9b::/96` (NAT64) are decoded to their embedded IPv4 and rejected when the inner IPv4 falls in any blocked range
  The guard runs both at API submission (rejects IP-literal URLs synchronously) and after DNS resolution at connect time (catches DNS rebinding). Flip to `true` for internal monitoring where private targets are the goal — operators are then responsible for network segmentation.
- **`security.credentials_kek_base64`** enables AES-256-GCM encryption of HTTP `basic_auth` and `bearer_token` values inside the `targets.check_spec` JSONB column. Generate with `openssl rand -base64 32`. Each write produces a fresh 12-byte random nonce; the on-disk shape is `{"$enc":"v1:<nonce>:<ciphertext>"}`. When the key is unset the service logs a startup warning and stores credentials plaintext (dev-friendly upgrade path — existing plaintext rows continue to read after a key is provisioned). Rotation and KMS integration are out of scope for the current version; treat the KEK as long-lived and protect it via your secret-management of choice (env file with restricted mode, container secret, etc.). A malformed KEK fails the process at startup.
- **Per-IP rate limiting** on `/api/v1/*` is the reverse proxy's job, not an in-process config block. In-process limiting is per-org and per-user, driven by `[rate_limits]` and the plan's per-minute budgets: see [docs/api.md](api.md#rate-limiting).
- **TLS cert checks** (`type = "tls_cert"`) open a dedicated TCP+TLS handshake per probe — separate from the HTTP check path. Recommended `interval >= 3600` so probe traffic stays light. The check accepts any cert chain (the goal is to *report* expiry status, not enforce trust), so an expired or self-signed cert still produces a structured result rather than a generic handshake error.
- **Domain expiry checks** (`type = "domain_expiry"`) query RDAP via a process-shared outbound HTTPS client. The IANA bootstrap registry (`https://data.iana.org/rdap/dns.json`) is fetched lazily on first use and cached for process lifetime — a registry update or a transient bootstrap failure persists until restart. Registries rate-limit clients by source address, so `interval >= 43200` is enforced server-side and daily is typical. The user-supplied domain is never a connection target: it travels as a path segment to a server chosen from the IANA bootstrap, a static RDAP override table, or a static WHOIS table. The resolved addresses of those servers are still filtered by the SSRF guard at connect time, because a hijacked or misconfigured DNS answer can point a trusted hostname at a private address.
  - **WHOIS fallback.** Several ccTLDs publish no RDAP server. When the bootstrap and the override table both miss, the check falls back to WHOIS on port 43 against a static per-TLD server table. Only a missing server triggers the fallback: an RDAP transport failure is returned as-is, so a registry outage does not double outbound load. Some registries (`.de`, `.ch`, `.jp` and others) publish no expiry date through any transport; monitors for those are refused at creation with the reason rather than failing on every interval.
  - **Sticky last-good.** Each successful probe persists `(expiry_at, registrar, last_success_at)` to the `domain_expiry_state` table (PK `target_id`, denormalised `org_id`; every query filters on both). On a transient probe failure — throttle, timeout, registry 5xx, RDAP 404, network blip — the executor returns the cached verdict instead of flipping the monitor to Degraded/Down. For Up the customer-facing `error` field stays empty; Degraded/Down carry a `served_stale: …` annotation so operators can distinguish a stale serve from a fresh probe. Operators also see the staleness via the `uptimepage_domain_expiry_stale_served_total` counter.
  - **Staleness ceiling: 7 days.** A cached row older than 7d is treated as "registry unreachable for too long" and surfaces as a real `Error`, which is alert-eligible.
  - **Cross-tenant singleflight.** Concurrent probes for the same domain coalesce to one outbound registry request (RDAP or WHOIS). Cache TTL on the singleflight slot is 60s — short enough that each scheduled cycle still fetches fresh, long enough to absorb scheduler-jitter waves at scale. Counter: `uptimepage_rdap_singleflight_total{outcome="hit"|"miss"}`.
- **Notification channels** are no longer global config. They are per-org runtime resources (Slack / Discord / Teams / Google Chat webhooks, generic HTTP webhook, Telegram bot, WhatsApp Cloud API) created via the [`/api/v1/notification-channels` API](api.md#notification-channels); a target binds them by id in its `alerts` array. Transport secrets are sealed at rest with the credentials KEK and never echoed back. Slack POSTs `{ "text": "..." }`; the generic webhook POSTs the incident-notice JSON (plus any configured custom headers, optionally HMAC-signed — see [docs/api.md](api.md#notification-channels)). Notifications are driven by the incident engine and persisted per attempt, so delivery state survives a restart. The binding syntax and the monitor-level firing policy (confirmations, recovery, reminders, region quorum) are documented in [docs/api.md](api.md#alert-config).
- **`api.cors`** opens `/api/v1/*` to browser-origin access. Each entry in `allowed_origins` must be a full origin (`https://app.example.com`) — wildcards are not parsed; set `allow_any_origin = true` to send `Access-Control-Allow-Origin: *` explicitly. The two are mutually exclusive — combining them or enabling CORS with an empty list aborts startup. `allowed_methods` is echoed in the preflight response (`Access-Control-Allow-Methods`); `Access-Control-Allow-Headers` is fixed to `content-type`, which is what the JSON API needs. `/healthz` and `/readyz` are not wrapped, so liveness probes are unaffected.
