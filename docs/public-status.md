# Public status page

The public status page is the customer-facing surface — an unauthenticated HTML page at `/status` plus a small JSON + RSS API under `/api/public/v1/*`. It's the only part of uptimepage that's safe to hand to the public, since nothing on it is org-private.

This page is for operators: how to publish a component, narrate an incident, and schedule a maintenance window. For the wire-level details of the underlying endpoints see [REST API](api.md#public-status-endpoints). For Caddy + the rate-limit plugin see [Deployment](deployment.md#public-status-surface).

> **Multi-tenant operators read this first.** This page describes the page itself; the workflow is identical on every page. In a multi-tenant deployment each org runs one or more pages at `{slug}.{base_domain}` — set `tenancy.subdomain_public_routes = true` and leave `tenancy.path_based_public_routes` off. The path-based `/status` surface is single-org and is for single-tenant deploys only (the default). See [Per-org status pages](per-org-status.md) for the routing, branding, and isolation model, and [Public status routing](configuration.md#public-status-routing) for the flag matrix.

## What's published vs what's private

By default every target is **private**. A monitor becomes a "component" on a status page only when it is curated onto that page — there is no per-target "public" flag. The aggregator filters at the SQL layer (a page renders only the monitors bound to it) and the wire types literally cannot serialise sensitive fields (`url`, `headers`, `basic_auth`, `bearer_token` are not part of any public schema), so a misconfiguration cannot leak credentials. That holds for the status page itself; the opt-in [detail link](#linking-a-component-to-its-detail-view) points at a share view that does publish the monitor's real name and checked address.

A monitor is published by adding it to a page; the per-page presentation lives on that binding, so the same monitor can appear on several pages under different names:

| Per-page field | Purpose |
|---|---|
| (binding exists) | the monitor appears as a component on that page |
| `public_name` | display name on this page; falls back to the operator-side monitor name when unset |
| `public_description` | optional one-liner shown under the component name |
| `public_group` | optional group label; components with the same value render as one block |
| `sort_order` | integer sort key (ASC); the reorder endpoint rewrites it. A group takes the position of its earliest component, so dragging any row of a group moves the whole block |
| `detail_link_enabled` | link the component name to that monitor's read-only detail view. Off by default |

A page belongs to an org and is managed by that org's owner; see [Per-org status pages](per-org-status.md) for the page model, the `max_status_pages` / `max_public_components` caps, and isolation.

## Enabling a component

The quickest path is the UI: open the page in **Settings → Pages → {your page}**. The editor lists every monitor in the org; toggle one **on page**, optionally set a **Public name** (blank shows the real monitor name), a **Group**, and whether the name carries a **Detail link**. Each edit autosaves via the components API below.

For scripting, add the monitor to the page, then set its per-page curation:

```bash
# Add monitor $TARGET_ID to page $PAGE_ID
curl -X POST https://app.uptimepage.dev/api/v1/status-pages/$PAGE_ID/components \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"target_id": "'$TARGET_ID'", "public_name": "Public API", "public_group": "Core APIs"}'

# Edit the per-page name / description / group later
curl -X PATCH https://app.uptimepage.dev/api/v1/status-pages/$PAGE_ID/components/$TARGET_ID \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"public_description": "Primary REST surface, all regions."}'

# Remove it from the page
curl -X DELETE https://app.uptimepage.dev/api/v1/status-pages/$PAGE_ID/components/$TARGET_ID \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN"
```

On the `PATCH`, `public_name`, `public_description`, and `public_group` use the same three-state semantics as incident narration: **omit** the field to leave it unchanged, send a string to set it, or send JSON `null` to clear it back to the default (real monitor name / no group). Blanking the field in the UI clears it for you.

## Linking a component to its detail view

Ticking **Detail link** adds an *uptime history* link beside the component, pointing at that monitor's [share link](share-links.md) — the read-only detail view with uptime, latency, recent results and incident history. The component's own name is never the link text, because a component named after a domain would read as a link to that site.

It is off by default, and deliberately so: the detail view is a different disclosure decision from the status strip, and a wider one. Credentials, headers and request bodies are masked, but that view publishes the monitor's **real name, its checked address and its tags** — the per-page `public_name` alias does not apply there. If you renamed a component to hide what it points at, this tick undoes that. Publishing a monitor's *status* and publishing its *history, timings and address* are separate decisions.

```bash
curl -X PATCH https://app.uptimepage.dev/api/v1/status-pages/$PAGE_ID/components/$TARGET_ID \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"detail_link_enabled": true}'
```

Enabling it mints a share link for the monitor if the component has none yet. That mint ignores the plan's share-link caps — the page already publishes the monitor, and `max_public_components` is the cap that governs it.

The monitor's share list shows which pages depend on each link, resolved live from the binding rather than copied into the link's label, so renaming or re-slugging a page can't leave a stale name behind. The tick itself is read the same way — a revoked or expired share shows as unticked, so the editor and the public page can never disagree.

The tick controls rendering, not the token's life:

- **Unticking** hides the link and keeps the share, so re-enabling gives back the same URL and a token you also pasted elsewhere keeps working.
- **Revoking the share** from the monitor's own share list is how you kill the URL. The component stays on the page and the tick clears; re-ticking mints a new token.
- **Removing the component** from the page revokes the minted share as well, so the URL dies with the binding rather than outliving it unattributed.

Adding a monitor that's already on the page is an idempotent no-op. Adding a brand-new monitor when the org is at its `max_public_components` cap is a quota error; a monitor already published on another page costs nothing to add here.

The page is cached for 10 s in-process (moka single-flight, with a second moka last-known-good cache so transient ClickHouse failures don't break the page). Changes appear on the next refresh.

## Narrating an incident

The background incident writer opens an incident automatically when a public target trips the threshold; it closes it again when checks recover. Both events happen without operator action. What's manual is the **narration** — the human-readable title, description, severity, and the running timeline of "investigating → identified → monitoring → resolved" entries that show up on `/status` and in the RSS feed.

Update the title + severity:

```bash
curl -X PATCH https://app.uptimepage.dev/api/v1/incidents/$INCIDENT_ID \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
  -H 'content-type: application/json' \
  -d '{
    "public_title": "Elevated 5xx in EU-WEST",
    "public_description": "Origin rollout regression — rolling back.",
    "severity": "major"
  }'
```

Sending JSON `null` for `public_title` or `public_description` clears the field and lets the page fall back to its auto-generated wording. Omitting the field leaves it unchanged.

Append a status update to the timeline:

```bash
curl -X POST https://app.uptimepage.dev/api/v1/incidents/$INCIDENT_ID/updates \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
  -H 'content-type: application/json' \
  -d '{
    "phase": "identified",
    "message": "Rolled back the offending deploy. Verifying recovery."
  }'
```

`phase` is one of `investigating`, `identified`, `monitoring`, `resolved`, `postmortem`. Posting `resolved` does **not** end the incident — the incident lifecycle is driven by check results, so manual "resolved" entries are advisory only. Posting an update to an already-ended incident is allowed (useful for postmortems).

Validation rules:

| Field | Rule | Error code |
|---|---|---|
| `public_title` | non-whitespace, ≤ 200 chars (use JSON `null` to clear) | `EMPTY_TITLE` / `TITLE_TOO_LONG` |
| `public_description` | ≤ 5 000 chars (use `null` to clear) | `DESCRIPTION_TOO_LONG` |
| `message` (update) | non-whitespace, ≤ 2 000 chars | `EMPTY_MESSAGE` / `MESSAGE_TOO_LONG` |
| `phase` (update) | exactly one of the five values above | `400` / `422` from the JSON extractor |

## Scheduling maintenance

A maintenance window is a planned outage. While the window is active, the page renders affected components as `Maintenance` (the truth-table rule is: maintenance dominates outage, so a real failure during the window still classifies as `Maintenance`, not `MajorOutage`). On the 90-day history strip, any day that overlapped a maintenance window renders as a maintenance cell rather than an outage cell.

Create:

```bash
curl -X POST https://app.uptimepage.dev/api/v1/maintenance \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
  -H 'content-type: application/json' \
  -d '{
    "title": "PG13 → PG16 cutover",
    "description": "Read-only for ~30 minutes.",
    "starts_at": "2026-05-14T22:00:00Z",
    "ends_at":   "2026-05-14T23:00:00Z",
    "component_ids": ["01a7b1ce-0000-7000-8000-000000000001"],
    "suppress_alerts": true
  }'
```

`suppress_alerts` defaults to `true`: while the window runs, its components page nobody. Incidents still open and uptime still records, so the history stays honest — only the paging is held, and the incident timeline says so. Held is not dropped: if the window ends and the incident is still open, the alert that was held goes out then. An incident you declare by hand still pages during a window. Windows created before this field existed keep alerting on, since that is what they were scheduled to do.

Set it to `false` for a window that is an announcement rather than planned downtime — a rolling upgrade where brief blips are expected but you still want waking if the service is genuinely down. The status page shows the window either way.

List, edit, delete:

```bash
curl 'https://app.uptimepage.dev/api/v1/maintenance?status=upcoming&limit=10'
curl -X PATCH https://app.uptimepage.dev/api/v1/maintenance/$ID \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
     -H 'content-type: application/json' \
     -d '{"title": "PG cutover (postponed)"}'
curl -X DELETE https://app.uptimepage.dev/api/v1/maintenance/$ID \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN"
```

Validation rules:

| Field | Rule | Error code |
|---|---|---|
| `title` | non-whitespace, ≤ 200 chars | `EMPTY_TITLE` / `TITLE_TOO_LONG` |
| `description` | ≤ 5 000 chars | `DESCRIPTION_TOO_LONG` |
| `ends_at` | strictly after `starts_at` | `INVALID_TIME_RANGE` |
| `ends_at - starts_at` | ≤ 30 days | `INVALID_DURATION` |
| `component_ids` | every id must reference an existing target | `INVALID_COMPONENT_ID` |
| `suppress_alerts` | boolean, defaults to `true` | — |
| PATCH on a window whose `ends_at` is already past | rejected | `422 MAINTENANCE_COMPLETED` |

For audit, prefer PATCHing a cancelled window's title (e.g. `"[cancelled] PG cutover"`) over hard-deleting historical entries.

Maintenance windows are managed through the API only in this release; there is no editor screen for them yet, so do not go hunting for one under Settings.

## What the public page renders

- **Banner** — one of `All Systems Operational`, `Maintenance in progress`, `Minor Service Disruption`, `Partial System Outage`, `Major System Outage`. Driven by the worst component state, with maintenance precedence as described above.
- **Component groups** — each component shows its current state, a 90-day history strip (one cell per day, oldest-first), and the operator-supplied description. A component with nothing recorded anywhere in the window reads `No data` rather than `Operational`: a heartbeat that has never been pinged has not been proven healthy, and saying otherwise is the worse of the two lies. An open incident still paints over it, so a declared incident on an unprobed component shows as the outage it is — provided you marked it as counting toward downtime. Publishing tells customers something happened; counting is the separate claim that the service was down, and only the second one colours a day or moves the component's dot. A published declaration you kept out of uptime is listed and marked on the strip without painting it. Silence carries no evidence, so it neither raises the banner nor holds it down.
- **Active and recent incidents** — operator-set `public_title` if present, otherwise an auto-generated `"<component> <status>"` string. Each incident links to a permalink at `/status/incidents/{id}` with the full timeline.
- **Maintenance** — active + the next 7 days of upcoming windows.
- **RSS feed** — `/api/public/v1/incidents.rss`. RSS 2.0; each item is a public incident with the latest update as the description.
- **Subscribe** — a header button that lets any visitor follow the page; see below.

## Subscriptions

Visitors can subscribe to a status page and get told about incidents and maintenance without polling the page. Two delivery channels ship today: email and webhook.

**Email** is double opt-in. Subscribing sends a confirmation link that expires after 24 hours; nothing is delivered until it is clicked. Confirmation mail is rate-capped per subscriber, per page, and per address per day, so the form cannot be used to bombard an inbox. Every email carries a one-click unsubscribe link (RFC 8058), no login needed.

**Webhook** verifies at subscribe time: the page sends a verification POST to the URL, and the subscription is created only if the endpoint answers 2xx. The response then shows a signing secret exactly once; store it. Every delivery is signed with the same scheme as notification-channel webhooks: an `X-Uptimepage-Timestamp` header and an `X-Uptimepage-Signature` HMAC over the timestamp and body, so the receiver can verify origin and freshness (see [REST API](api.md#notification-channels)).

Subscribers receive public incident updates as they are posted (payload `type` `incident_update`) and upcoming maintenance announcements (`type` `maintenance`). Internal incident activity is never sent; the feed carries exactly what the public page shows.

The page owner sees the subscriber list, with counts per channel, in the status-page editor, and can remove any subscriber there or via `DELETE /api/v1/status-pages/{id}/subscribers/{subscriber_id}`.

## Refresh behaviour

The page is statically rendered and works without JavaScript. With JS enabled, an HTMX `hx-trigger="every 30s"` swaps the dynamic region (the banner, the component grid, and the incident lists) without a full page reload. The chrome around it — header, footer, RSS link — stays put. A small (~35 LoC) `static/js/public/tz.js` helper rewrites ISO timestamps into the visitor's local timezone tooltip; everything else is plain HTML.

## Caddy and the rate-limit plugin

The public surface gets its own per-IP rate-limit zone through an `@public` matcher in `deployment/Caddyfile`. The matcher also applies a per-IP rate limit (60 requests / minute), which requires the [`caddy-ratelimit`](https://github.com/mholt/caddy-ratelimit) plugin. The stock `caddy:2-alpine` image doesn't include it — build a `custom-caddy:2` image once via `xcaddy`. The procedure is in [Deployment](deployment.md#public-status-surface) and [`deployment/README.md`](https://github.com/uptimepage/uptimepage/tree/main/deployment).

If you'd rather not maintain a custom Caddy image, comment out the `rate_limit { … }` block in the Caddyfile. The public surface still serves; you just lose per-IP throttling. Putting Cloudflare in front of Caddy is the other option.

## Embeddable status badge

`GET /api/public/v1/badge.svg` returns a shields.io-style SVG badge that operators can embed in README files or external dashboards. Two modes:

```markdown
<!-- Overall page status -->
![status](https://acme.uptimepage.dev/api/public/v1/badge.svg)

<!-- Single component -->
![api status](https://acme.uptimepage.dev/api/public/v1/badge.svg?component=<uuid>)
```

The badge reuses the cached page payload, so it tracks the `/status` view inside the 10-second cache window. Unknown component ids return `404` with the public error envelope; only `style=flat` is recognised (others return `400`).

The page editor renders ready-to-copy markdown for the overall badge and each on-page component. The copyable URL is built from the page's public origin, so on path-based/self-host deploys set `auth.public_base_url` to the externally reachable URL (the same value subscriber links need); otherwise the badge URL points at `localhost`.

`?component=<uuid>` works for any public component regardless of check type — an HTTP, DNS, or TLS-certificate monitor each gets its own badge that reflects that component's current status.

## Common questions

**Can I have a component that's public but doesn't trigger incidents?** No. Incident materialisation walks the same binding the page does — a monitor on any enabled page is eligible for incidents. If you want a check that's published but not alerting, set `enabled = false` on the alert channels — the incident will still open, but no notification fires.

**Can I publish a maintenance window without listing the affected components?** No. `component_ids` may be empty in the request body, but the aggregator filters maintenance windows that touch zero public components out of the page (and out of the JSON), so they wouldn't appear anywhere. List at least one public component.

**What's the cache TTL?** 10 s. Single-flight: only one task computes the page when the entry expires; others wait for the result. On ClickHouse failure the last-known-good snapshot serves until the next successful compute.

**How long does the 90-day history go back?** Exactly 90 days, oldest day on the left. Cells with no recorded checks render as `NoData` (grey); the aggregator does not fabricate data.

**Is there an Atom feed?** No, RSS 2.0 only. Most feed readers consume both.
