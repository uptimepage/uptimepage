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

The detail view opens on the page's own address, its subdomain or custom domain, in the page's branding: logo, display name, brand colour, style, and the powered-by footer only when the page shows it. That host answers only for the detail links its own page shows; any other share link is a 404 there. A page's link opened on the app host redirects to the page. A share link minted from the monitor itself keeps the app's layout.

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

The background incident writer opens an incident automatically when a public target trips the threshold; it closes it again when checks recover. Both events happen without operator action, and each posts a system update to the timeline (`Automatically detected …`, `Automatically resolved …`) so subscribers hear the start as well as the end. What's manual is the **narration** — the human-readable title, description, and the running timeline of "investigating → identified → monitoring → resolved" entries that show up on `/status` and in the RSS feed.

The public page labels every incident by **impact** (`Degraded`, `Partial outage`, `Major outage`), the same words the history strip uses. A monitor-opened incident's impact is measured: degraded checks read `Degraded`, a failure with some region still answering reads `Partial outage`, every region failing reads `Major outage`. A declared incident's impact comes from its `severity` (`minor` → `Degraded`, `major` → `Partial outage`, `critical` → `Major outage`). `severity` on a monitor-opened incident drives paging and the operator console, not the public label.

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

A maintenance window is a planned outage. While the window is active, the page renders affected components as `Maintenance` (the truth-table rule is: maintenance dominates outage, so a real failure during the window still classifies as `Maintenance`, not `MajorOutage`). On the 90-day history strip, any day that overlapped a maintenance window renders as a maintenance cell, and time inside a window is not downtime on the public page: a failure inside it neither colours the day nor lowers the uptime figure, whether or not the window holds paging. A monitor added to a window that is already running counts from when it was added, and one dropped from it counts until it was dropped, so a failure while the monitor was out of the window is downtime. A failure outside the window on the same day still paints the day, since an outage outranks maintenance. The app's own monitor and incident views keep counting that time, so you still see that the work broke something.

Open **maintenance** in the top navigation to schedule one. Pick the monitors, a start and an end in your own timezone, and whether to hold paging. Active and upcoming windows are listed first, and completed ones sit under **past** as read-only history. Each window has its own page with its status updates. An upcoming window can be edited or cancelled. An active one can be edited or ended now, which moves it to past straight away. Everything the screen does is also available through the API and the [MCP server](mcp.md).

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
curl -X PATCH https://app.uptimepage.dev/api/v1/maintenance/$ID \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
     -H 'content-type: application/json' \
     -d '{"ends_at": "1970-01-01T00:00:00Z"}'
curl -X DELETE https://app.uptimepage.dev/api/v1/maintenance/$ID \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN"
```

Validation rules:

| Field | Rule | Error code |
|---|---|---|
| `title` | non-whitespace, ≤ 200 chars | `EMPTY_TITLE` / `TITLE_TOO_LONG` |
| `description` | ≤ 5 000 chars | `DESCRIPTION_TOO_LONG` |
| `ends_at` | strictly after `starts_at` | `INVALID_TIME_RANGE` |
| `ends_at` | on create and on every edit, must be in the future, except to end a running window now (below) | `INVALID_TIME_RANGE` |
| `ends_at - starts_at` | ≤ 30 days | `INVALID_DURATION` |
| `component_ids` | every id must reference an existing target | `INVALID_COMPONENT_ID` |
| `suppress_alerts` | boolean, defaults to `true` | — |
| PATCH on a window whose `ends_at` is already past | rejected | `422 MAINTENANCE_COMPLETED` |
| PATCH on a cancelled window | rejected | `422 MAINTENANCE_CANCELLED` |
| DELETE on a window whose `ends_at` is already past | rejected | `422 MAINTENANCE_COMPLETED` |

To end a running window now, PATCH an `ends_at` at or before the current time and leave `starts_at` alone. The server sets `ends_at` to its own clock, so a script or a browser with a wrong clock cannot end the window at the wrong moment. The same `ends_at` on a window that has not started, or together with a new `starts_at`, is rejected with `INVALID_TIME_RANGE`. Because a past `ends_at` on a running window ends it at once, check the offset of any timestamp you send.

Cancelling is a soft delete. The window stops showing on the page, stops holding paging and stops counting toward the quota, but it stays listed under **past** with `deleted_at` and `deleted_by` set, and `status=past` and `status=all` include it. Only a window that has not ended can be cancelled. Cancelling it again returns `404`, and editing it returns `422 MAINTENANCE_CANCELLED`. Subscribers who were already told about the window are not sent a cancellation, and edits made after the announcement are not re-sent either; to tell them something, post an update.

Every window records who scheduled it (`created_by`), who changed it last (`updated_by`) and, once cancelled, who cancelled it (`deleted_by`). Creating, editing (which includes ending) and cancelling a window also write `maintenance.created`, `maintenance.updated` and `maintenance.cancelled` entries to the organization audit log.

### Status updates

Post an update when there is something to say about a window: the work runs late, it finished, or something changed. The status page shows the newest one under the window while it is upcoming or running, and the window's history row reads as its latest update and opens to the description and every update. Each update also goes to the page's email and webhook subscribers.

```bash
curl -X POST https://app.uptimepage.dev/api/v1/maintenance/$ID/updates \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"message": "The data copy is slower than planned. We now expect to finish by 23:30 UTC."}'
```

An update records where the window stood when it was posted (`scheduled`, `in_progress` or `completed`, the words the automatic notices use), and keeps that phase if the window moves later. A completed window keeps its own fields read-only but still takes updates, which is how to add a closing note such as when the work really ended. Updates cannot be edited or removed, and `GET /api/v1/maintenance/{id}` returns them oldest first.

| Rule | Error code |
|---|---|
| `message` non-whitespace, ≤ 2 000 chars | `EMPTY_MESSAGE` / `MESSAGE_TOO_LONG` |
| a cancelled window takes no updates | `422 MAINTENANCE_CANCELLED` |
| at most 50 updates per window | `422 MAINTENANCE_UPDATE_LIMIT` |

Posting writes a `maintenance.update_posted` entry to the audit log.

## What the public page renders

- **Banner** — one of `All Systems Operational`, `Maintenance in progress`, `Minor Service Disruption`, `Partial System Outage`, `Major System Outage`. Driven by the worst component state, with maintenance precedence as described above.
- **Component groups** — each component shows its current state, a 90-day history strip (one cell per day, oldest-first), an uptime figure, and the operator-supplied description. Uptime is time-based: confirmed-incident downtime over the time the component has been probed within the window, so a four-minute outage in ten days of data reads 99.97%, not "nine good days out of ten". Downtime is weighted as the day colours weight it, below, and overlapping incidents on one component count once. A component with nothing recorded anywhere in the window reads `No data` rather than `Operational`: a heartbeat that has never been pinged has not been proven healthy, and saying otherwise is the worse of the two lies. An open incident still paints over it, so a declared incident on an unprobed component shows as the outage it is — provided you marked it as counting toward downtime. Publishing tells customers something happened; counting is the separate claim that the service was down, and only the second one colours a day or moves the component's dot. A published declaration you kept out of uptime is listed and marked on the strip without painting it. Silence carries no evidence, so it neither raises the banner nor holds it down.
- **Day colours** — a day's colour says how long the component was down that day, not only the worst state it touched, so a four-minute blip does not paint the day the way a lost afternoon does. Downtime is weighted the way Atlassian Statuspage weighs it: a partial outage counts for 30% of its length, degraded performance not at all, and an instant two incidents cover counts once. Under 20 minutes reads yellow, 20 to 60 orange, an hour or more red; any disruption keeps the day off green. The uptime figure beside the strip weights downtime the same way, as the dashboard and the monitor pages do. Hovering or focusing a day lists the time it spent in each state, worst first, with the incidents behind it. The public API's `history` still reports each day's worst state.
- **Active and recent incidents** — operator-set `public_title` if present, otherwise an auto-generated `"<component> <status>"` string. Each incident links to a permalink at `/status/incidents/{id}` with the full timeline. While an incident waits out its monitor's recovery period, its component reads operational and the incident reads Monitoring, without the Ongoing badge; the phase goes back to what the timeline last said if the failure returns. A phase you set yourself, Monitoring, Resolved or Postmortem, stands, and the timeline keeps every update as written.
- **History** — the incidents and finished maintenance windows that started in the last 30 days, as compact rows, newest first. Tabs above the rows show all of them, only incidents or only maintenance (`?show=incidents`, `?show=maintenance`), and the tab holds through the page's refresh. The section is there even on a quiet page, with a line saying nothing was resolved or completed, and it always links the archive. An incident row shows its impact, title, component, start time in the visitor's own timezone, and how long it lasted. Incidents on one component less than an hour apart, and incidents on different components that overlap and began within 15 minutes of each other, fold into one row that opens to the incidents inside it; its length counts only the time something was down, not the healthy gaps between. A flapping service, or one fault taking several components down, then reads as one disruption rather than a wall of cards. An incident you posted an update to keeps its own row and shows your latest words. The lines the platform writes for you are left out: the detected and resolved notes, and the default line a resolve or publish posts when you give none. A maintenance row shows the window's title, the components on the page it covered, when it started and how long it ran, with its latest update or else its description; a row with updates, or with a long description, opens in place to the description and every update. Cancelled windows are never listed. What the page showed stays in its history: an incident or a started window keeps its row, under the name the page gave the monitor, when that monitor is removed from the page, deleted, or dropped from a running window. Rows that ended in the last 7 days are open and older ones sit behind one toggle, and a row a visitor opened stays open through the page's 30-second refresh. The archive at `/status/incidents` covers the last 90 days, lists each month the same way with the same tabs, and includes an incident still open and a window still running; a window that began earlier and ran into those days is listed under the month it began.
- **Maintenance** — active + the next 30 days of upcoming windows, each with its newest update. Start and end show in the visitor's own timezone with the zone named, since readers plan around them. Once a window ends it moves to the history.
- **RSS feed** — `/api/public/v1/incidents.rss`. RSS 2.0; each item is a public incident with the latest update as the description.
- **Subscribe** — a header button that lets any visitor follow the page; see below.

## Language

Each page has a language: English by default, or German. Set it under **Language** in the page editor, with `PATCH /api/v1/status-pages/{id}` and `{"public_locale": "de"}` (see [Page identity and branding](per-org-status.md#page-identity-and-branding)), or with `locale` on [`uptimepage_status_page`](terraform.md); `en` switches back.

The language covers what the page writes itself: status labels, headings and buttons, dates and durations, the title of an incident you never named, the status badge, the RSS feed's own title, the subscribe flow, and the confirmation, incident and maintenance emails sent to subscribers. Times in those emails stay in UTC. Text you write is shown as written: page, component and group names, the about text, incident titles and updates, and maintenance windows.

A German page sends `Content-Language: de` and formats dates the German way in every browser. An English page formats dates the way the visitor's browser does. The RSS feed declares the page's language, and the public JSON API keeps its state keys (`operational`, `investigating`) in English. One page has one language, so for two audiences run two pages.

## Who can open a page

A page is in one of three states, set in the editor's rail under **who can open it**:

| State | Who reads it |
|---|---|
| public | anyone with the URL |
| private | only people a way in lets through: an address or domain on the access list, or anyone with the page password |
| off | nobody: the URL 404s and subscribers get no mail |

Private pages come with the Team plan; a self-hosted install has them whatever its plan says. A private page has two ways in, the **email code** and the **page password**, and any one that is on lets a visitor in; at least one has to be. They live in the editor's **access** tab, or in the `access` field of the page in the [REST API](api.md#operator-endpoints-status-pages), the MCP tools and Terraform. The email code's list takes a domain such as `example.com`, which lets in every address at it, or an address, which lets in one person, up to 500 entries. Org members get no shortcut: someone who wants to read the page adds their own address or uses the password like anyone else.

**Signing in.** A visitor to a private page sees a sign-in screen in the page's branding and language and enters their email. Every address gets the same answer, and only one the list covers, and that is not blocked, gets a mail: a 6-character code for the tab they started in, and a link for another device. Both work once and for 15 minutes; the link shows a confirm button first, so a mail scanner that opens it spends nothing. An address gets at most one mail a minute and ten a day from one page. Addresses that have never signed in to the page also share a budget of 500 mails a day per page and 20 per requesting IP; people who signed in before never wait on it, so made-up addresses at a listed domain cannot lock them out. Signing in sets a cookie for that page's own host only.

**Passkeys.** After the first code, a viewer can save a passkey in their password manager or on the device and sign in with it next time, from the button or the browser's autofill. The owner can switch passkeys off, which refuses both ceremonies and keeps what was saved. **Ask passkey users for an email code again** (30 days, 90 days, a year, or never) makes a passkey sign-in fall back to the email code once the viewer last proved their address longer ago than that; it applies at the next sign-in, never to an open session. A passkey belongs to the host it was saved on, so a new custom domain means one more email sign-in and a new passkey. A page served from the app's own host (a path-based deploy) offers the email code only, so its passkeys never mix with the app's.

**The page password.** One password the owner shares with everyone who should read the page: people with no shared email domain, a partner's NOC, a wall screen. It cannot tell people apart, so there is no one to block; changing it is how everyone is shut out. It is stored as a hash, so it is never shown again, only replaced: copy it when you type or generate it. Any length from 1 to 128 characters is accepted as typed; under 12 the editor suggests a generated one (20 characters, about 116 bits, drawn in your browser). Turning the password off keeps it, and turning it on again brings the same one back. A changed password, or turning it off, signs out every device that came in with it and ends the subscriptions it let in. **Sign them all out** ends the password sessions open right now but leaves the password working. Ten wrong tries in 15 minutes from one network (an IPv6 client counts by its /64) are allowed; the next gets `429` with `Retry-After` and the wait in minutes on the sign-in screen, until the oldest try is 15 minutes old. Only that network waits: the page itself never locks, so wrong tries from elsewhere cannot lock out every reader. The password form names the page's host as its username, so a password manager keeps one entry per page.

**Sessions.** One length applies to both ways in: until sign-out by default, or 7, 30 or 90 days; shortening it applies to sessions already open. An email session also ends the moment the viewer is blocked, the list stops covering them, or the email code is turned off; a password session when the password changes or is turned off. Any session ends after a year unused.

**What is gated.** Every surface of the page answers only someone signed in: the page and its refresh, the incident history and detail pages, the JSON API, the badge and the feed. Anyone else gets the sign-in screen or `401 PAGE_SIGN_IN_REQUIRED`. Each of these answers carries `Cache-Control: private, no-store` and `noindex`, so no shared cache keeps it and no search engine lists it, whatever **hide from search engines** says. The password is never taken in a URL or a header, where it would land in logs and browser history; scripts use `/api/v1` with a token. For readers that carry no cookie the page's RSS link is a feed link: a viewer's own, which stops working when they are blocked or leave the list, or for a password visitor the page-password link, which stops when the password changes and answers nothing while it is off. The logo stays open, since the sign-in screen shows it. A component's [detail link](#linking-a-component-to-its-detail-view) is protected by its own token as before; turn detail links off on a private page's components if nothing should be reachable without signing in.

**Subscribers.** Making a page private removes every webhook subscriber and every email subscriber no way in covers. A signed-in viewer subscribes as themselves, with no confirmation mail, and the subscription ends when they are blocked or a list change stops covering them. A password visitor subscribes an address, confirmed by email as on a public page; the editor's subscriber list marks it **via password**, and it lasts while the same password is on, unless the list covers the address anyway. A password changed while the page is public ends those subscriptions when the page is next made private.

**Viewers.** The access tab lists who signed in by email, when they last visited, whether they saved a passkey, and their history: the days they opened the page and each sign-in, kept for a year. **Block** signs someone out at once, removes their passkeys and subscription, and keeps them out whatever the list says; **unblock** lifts only the block. Password visitors are anonymous, so the password has one line: devices signed in with it, its last sign-in and the wrong tries in the last 24 hours.

**Plan changes.** If the plan stops covering private pages, a private page is held (it 404s) rather than opened to everyone, and comes back private when the plan covers it again or public once its owner makes it so. Its access stays editable while it is held. Making it public keeps the list, the viewers and the password for next time, and ends every session.

## Subscriptions

Visitors can subscribe to a status page and get told about incidents and maintenance without polling the page. Two delivery channels ship today: email and webhook.

On a [private page](#who-can-open-a-page) only someone signed in subscribes, and only by email: a viewer as themselves, with no form and no confirmation mail, and a password visitor with the confirmed form below.

**Email** is double opt-in. Subscribing sends a confirmation link that expires after 24 hours; nothing is delivered until it is clicked. Confirmation mail is rate-capped per subscriber, per page, and per address per day, so the form cannot be used to bombard an inbox. Every email carries a one-click unsubscribe link (RFC 8058), no login needed.

**Webhook** verifies at subscribe time: the page sends a verification POST to the URL, and the subscription is created only if the endpoint answers 2xx. The response then shows a signing secret exactly once; store it. Every delivery is signed with the same scheme as notification-channel webhooks: an `X-Uptimepage-Timestamp` header and an `X-Uptimepage-Signature` HMAC over the timestamp and body, so the receiver can verify origin and freshness (see [REST API](api.md#notification-channels)).

Subscribers receive public incident updates as they are posted (payload `type` `incident_update`), maintenance announcements when a window is scheduled and when it completes (`type` `maintenance`, `phase` `scheduled` or `completed`), and the updates posted to a window (`type` `maintenance_update`, `phase` `scheduled`, `in_progress` or `completed`). Internal incident activity is never sent; the feed carries exactly what the public page shows.

**One mail per outage.** Monitors on a page that fail together reach an email subscriber as one outage, not one mail per monitor. Incidents belong together when they overlap and began within 15 minutes of each other, the same rule the page history folds by. The first mail goes a minute after the first failure and lists every component affected by then, each linking to its incident. The second goes once every component is back and the page has stayed quiet for 5 minutes, and says how long each was down; the same monitor failing again inside those 5 minutes stays part of the outage, so a flapping monitor sends one pair of mails. An outage that was over within its first minute sends only the second mail. Five monitors down for two minutes send each subscriber two mails instead of ten. Updates someone writes, and every update of an incident you declared yourself, still go out one by one, as written. Webhook subscribers are unchanged: they get every incident update, the automatic ones included.

**When a delivery fails.** It is tried again. A webhook endpoint that answers `429` or `503` with a `Retry-After` in seconds is asked again after that wait, at least 30 seconds and at most an hour later, and the wait uses none of its 8 tries. `410 Gone` ends the delivery at once. Any other answer outside 2xx, a timeout or a refused connection uses a try, and the next comes 30 seconds later, doubling up to 30 minutes, spread a little at random so endpoints that failed together are not hit together again. After the eighth the delivery is given up. Email is never given up for the mail provider's trouble: rate limits and quotas wait for the time the provider names, and an outage is retried on the same backoff, because none of it says anything about the address. A mail the provider refuses on its own, such as one to an invalid address, is not tried again. Anything still unsent a day after the event is dropped as no longer news.

Subscriber mail goes out through the same send budget as the rest of the product's mail, 100 recipients to a request, after sign-in links and alerts. A subscriber who confirms after something was posted does not get it.

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
