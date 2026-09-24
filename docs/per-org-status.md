# Per-org status pages

Each org owns one or more public status pages. A page lives at `{slug}.{base_domain}` in SaaS mode (`acme.uptimepage.dev`, `status.acme.uptimepage.dev`, …, apex-wildcard shape) and renders **only** the monitors that org has curated onto it, with that page's branding, incidents, and maintenance. An org starts with no pages; the owner creates one from the settings screen, which mints it published at a temporary slug and opens the editor to set the real one. Pages can be renamed, added to, or taken offline at any time.

The number of pages an org can run is plan-capped (`max_status_pages`); the free (Standard) plan gets one. Multiple pages let an org split surfaces — e.g. a public page and a separate internal-stakeholder page — each showing a different subset of monitors under a different URL.

This page is the per-org / per-page model. For the component, incident, and maintenance workflow (identical on every page) see [Public status page](public-status.md). For the wildcard cert and reverse-proxy setup see [Deployment](deployment.md#public-status-surface) and the full runbook in [`deployment/README.md`](https://github.com/uptimepage/uptimepage/tree/main/deployment).

## When it applies

| Shape | Config | Public surface |
|---|---|---|
| Single-tenant | `tenancy.path_based_public_routes = true` (default) | the lone org's default page, served path-based at `/status` on the operator host |
| Multi-tenant SaaS | `tenancy.subdomain_public_routes = true`, `tenancy.path_based_public_routes = false` | every enabled page at `{slug}.{base_domain}` |

Single-tenant deploys never pay the subdomain path: there is one live org, so its default page is mounted on the operator host at `/status`.

Path-based and subdomain public routes are mutually exclusive — serving `/status` on the operator host alongside subdomains would publish one page's data at every tenant's expected URL. Pick one.

## Host routing

A page is resolved from the request `Host` header, not the path. The slug names a **page**, not an org; the lookup admits only enabled pages whose org is not soft-deleted.

| Host | Result |
|---|---|
| `acme.uptimepage.dev`, page enabled | that page |
| `acme.uptimepage.dev`, page disabled (draft) or org soft-deleted | **404** |
| `nope.uptimepage.dev`, no such page slug | **404** |
| `a.b.uptimepage.dev` (extra label) | **404** |
| `uptimepage.dev` (no slug label, bare base) | **404** |
| missing `Host` header | **404** |

A page slug is globally unique (it routes a subdomain), so two orgs can never claim the same slug. `base_domain` must be a multi-label domain (it needs at least one dot); the boot assertion refuses an empty or single-label value, because a loose base would let the slug extractor match arbitrary `Host` headers.

The apex wildcard `*.{base_domain}` DNS record plus a wildcard TLS cert (Let's Encrypt via the Hetzner DNS-01 challenge) means a new page works the instant it is enabled — no per-page DNS or cert step. Operator subdomains (`app.{base_domain}`, `mail.{base_domain}`, …) use explicit DNS records that take precedence over the wildcard, and the operator host is kept on its own per-host cert.

## Managing pages

The org **owner** manages pages from the operator UI at `/settings/pages` (a list to create / rename / publish / delete pages) and the per-page editor at `/settings/pages/{id}` (URL slug, branding, logo, and which monitors appear). The same operations are available over the API:

| Endpoint | Purpose |
|---|---|
| `GET /api/v1/status-pages` | list this org's pages |
| `POST /api/v1/status-pages` | create a page (capped at `max_status_pages`) |
| `GET /api/v1/status-pages/{id}` | one page + its live URL and logo URL |
| `PATCH /api/v1/status-pages/{id}` | rename, change slug, publish/unpublish, edit branding |
| `DELETE /api/v1/status-pages/{id}` | delete the page (its component bindings cascade) |
| `GET /api/v1/status-pages/{id}/components` | the monitors curated onto the page |
| `POST /api/v1/status-pages/{id}/components` | add a monitor to the page |
| `PATCH /api/v1/status-pages/{id}/components/{target_id}` | set per-page name / description / group |
| `DELETE /api/v1/status-pages/{id}/components/{target_id}` | remove a monitor from the page |
| `POST /api/v1/status-pages/{id}/components/reorder` | set the component order |
| `POST /api/v1/status-pages/{id}/logo` | upload a logo (multipart) |
| `DELETE /api/v1/status-pages/{id}/logo` | remove the logo |

Every route is scoped to the caller's active org: a page id that isn't in that org resolves to **404** (the same cloak as the rest of the API), so an owner of one org can neither see nor mutate another org's page.

### Page identity and branding

| Field | Rule | Default when unset |
|---|---|---|
| `name` | 1–80 chars; the operator-facing label in the Pages list (not shown publicly) | — (required) |
| `slug` | globally-unique subdomain slug; 3–30 chars, lowercase letters / digits / hyphens, starts with a letter. A rename is a hard cutover — the old URL stops working immediately | — (required) |
| `enabled` | published? a draft (`false`) 404s on its public host | off on create via the API; the settings screen creates it on, at a temporary slug |
| `public_display_name` | 1–80 chars | the org's name |
| `public_brand_color` | `#RRGGBB` (6-digit hex) | `#3b82f6` |
| `public_about` | Markdown, ≤ 500 chars, rendered to sanitised HTML | omitted |
| `public_style` | one of the named themes | `default` |
| `public_show_powered_by` | footer attribution toggle. Honoured only on plans with `white_label_enabled`; on any other hosted plan the badge always renders, whatever this is set to | on |
| `public_website_url` | `http(s)` address, ≤ 200 chars. The header logo (or display name) links here, so a reader who arrived from your site can get back. The link carries `rel="nofollow"` unless the plan sells white-label (see [Quotas](quotas.md#the-seeded-plans)) | header links to the status page root |
| `public_hide_from_search` | serve the page, its incident pages and the archive with `noindex`. The URL keeps working for anyone who has it | off (indexable) |
| logo | PNG / JPEG / WebP, ≤ 1 MB, ≤ 1200 px; larger images are downscaled. Format is sniffed from the bytes (declared content-type ignored — a script/SVG can't masquerade as an image) and the decoder is allocation- and dimension-bounded against decompression bombs | header shows the display name as text |

A `PATCH` with a `branding` object replaces the display fields wholesale; `name`, `slug`, and `enabled` are independent partial fields. The logo has its own endpoints and is never touched by a branding edit. The editor shows the live URL so the owner can preview exactly what visitors see.

### Curating components

A monitor appears on a page only while a `status_page_components` binding exists for that `(page, target)` pair. Adding the monitor in the editor creates the binding; removing it deletes the binding. The per-page curation lives on the binding, so the same monitor can sit on several pages under different names:

| Per-page field | Purpose |
|---|---|
| `public_name` | display name on this page; falls back to the operator-side monitor name when unset (1–80 chars) |
| `public_description` | optional one-liner under the component name (≤ 200 chars) |
| `public_group` | optional group label; same value clusters together, ungrouped renders last (≤ 50 chars) |
| `sort_order` | integer sort key within a group (ASC); the reorder endpoint rewrites it |

The per-page **distinct-target** cap is `max_public_components`: it counts unique monitors across all of the org's pages. A monitor already published on one page costs nothing to add to another; a brand-new monitor at the cap is rejected with a quota error. Adding a monitor already on the page is an idempotent no-op; adding a page or target that isn't in the caller's org is a 404, not a quota error.

### About text

`public_about` is Markdown. It is parsed and then run through an HTML sanitiser before it ever reaches a template: only `p`, `strong`, `em`, `a`, `br`, `ul`, `ol`, `li` survive, links get `rel="noopener nofollow"`, and there is no raw-HTML escape hatch. Scripts and inline styles are stripped.

### Brand colour

The colour is validated at three independent layers — the database constraint, the application validator, and again in the template right before it is written into the page's `<style>`. Any value that isn't a strict 6-digit hex falls back to the default at render time, so a relaxed constraint at one layer can't open a CSS-injection path on its own.

### Logo storage

An uploaded image's format is detected from its **bytes**, not its declared content type. The on-disk filename is derived from the page and a hash of the content, never from anything the client sends, so a crafted filename can't escape `public_status.logo_dir`. Replacing or removing a logo deletes the previous file.

## Caching and turning a page off

Each rendered page is cached for `public_status.cache_ttl_secs` (default 10 s), keyed by page id. A separate last-known-good layer keeps the most recent successful render per page so a transient Postgres/ClickHouse blip serves slightly stale data instead of an error. That layer is bounded by `cache_max_orgs` and idle-evicts after `last_good_ttl_secs`, so churn through many pages can't grow it without limit.

Unpublishing a page (`enabled` → false) makes the host resolver stop resolving its slug; the cache entry idles out, so the page is a 404 within one TTL window at most. Deleting a page or soft-deleting the org has the same effect (the purge worker handles the org case).

## Security model

- **Published only.** The public host resolver admits a page only when it is enabled and its org is not soft-deleted. A draft or deleted page's slug resolves to 404 even though the string still exists. The authenticated org lookup is a separate function and is never used on the public path.
- **Operator sessions never reach status subdomains.** The session cookie is host-only (`auth.session.cookie_domain = ""`), so the browser scopes it to the operator host and never sends it to `*.{base_domain}`. The binary refuses to boot if `cookie_domain` is set to a parent zone that would overlap the apex wildcard.
- **No operator surface on the page.** The status page renders no operator UI, sets no cookies, and never echoes request auth headers.
- **Tenant isolation.** A request for one page returns only that page's curated monitors; the page cache and every data source are keyed by page id, and the underlying queries bind the org id, end to end. A monitor not bound to the page is never queried for it, so its operator-side name can't leak.

## Configuration

The `[public_status]` block and the split tenancy flags are documented in [Configuration → Public status page](configuration.md#public-status-page) and [Configuration → Public status routing](configuration.md#public-status-routing).

## Custom domains

Every page is served under the shared `*.{base_domain}` apex wildcard. On a plan with `custom_domain_enabled` (Pro and Team on the hosted service), a page can also be served on the org's own hostname, such as `status.theirbrand.com`. On the hosted service, setup is by email for now: write to hello@uptimepage.dev with the hostname. On a self-hosted instance, the operator first switches to subdomain mode as `deployment/.env.example` describes (it needs the wildcard certificate's DNS token), then sets `UPTIMEPAGE_CUSTOM_DOMAINS_ENABLED` in `.env` and keeps the Caddyfile blocks marked `SELF-HOST` (see [Custom domains](https://github.com/uptimepage/uptimepage/tree/main/deployment#custom-domains) in the deployment README). The org's plan must allow it too: the org seeded from `bootstrap.email` gets `quotas.default_plan` (`team` by default, which does), while an org created later by signup starts on `founding` or `free`, neither of which does. There is no settings form yet on either: the steps below are direct updates to the page's `custom_domain`, `custom_domain_verified_at` and `custom_domain_activated_at` columns.

- The org adds a `CNAME` for its hostname to the target it is given.
- The operator sets `custom_domain` to the hostname and stamps `custom_domain_verified_at`. Within 30 seconds the running instances pick it up, and Caddy issues a certificate for that name on the first HTTPS request (on-demand TLS, gated by the `ask` endpoint in [Configuration](configuration.md#sections), which answers 200 only for a verified hostname on a plan that allows it).
- Once the page loads over HTTPS on the new name, the operator stamps `custom_domain_activated_at`. Only then do subscriber mail, the feed, `og:url` and the canonical tag switch to the custom hostname; until then they keep the subdomain, so no link points at a host that has not completed a TLS handshake.

The subdomain keeps working alongside the custom hostname. A host that is neither a subdomain nor a verified custom domain gets a 404, never the page or the operator app. Apex domains are not supported, because an apex cannot carry a `CNAME`.
