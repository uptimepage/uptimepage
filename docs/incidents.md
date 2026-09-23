# Incident management

uptimepage turns a failing check into a first-class operational incident: a tracked lifecycle with acknowledgement, ownership, paging, on-call rotations, escalation, and a retrospective — not just a banner on a status page. This page is for operators running incident response. For the customer-facing surface it publishes to, see [Public status page](public-status.md); for the wire-level endpoints see [REST API](api.md).

## The core idea: internal state is not public phase

The single most important distinction is that what your responders see is orthogonal to what your customers see. Conflating the two is the classic incident-tooling bug, so uptimepage keeps three independent axes on one incident:

| Axis | Values | Audience | Changed by |
|---|---|---|---|
| **Internal state** | `triggered` → `acknowledged` → `resolved` | Responders | Acknowledge / resolve / reopen actions |
| **Public phase** | `investigating` / `identified` / `monitoring` / `resolved` / `postmortem` | Customers on a status page | Operator-posted public updates only |
| **Visibility** | `internal` / `public` | — | An explicit publish action |

Acknowledging an incident stops escalation and records who took it — it posts **nothing** to a status page. Customers see something only when you publish the incident and post a public update. An incident can run its whole internal lifecycle while staying `internal`.

## How an incident opens

A background writer scans every enabled monitor (not only status-page components). When a monitor sustains a bad state — `down`, `error`, or `degraded` — it opens one incident; a sustained recovery to `up` resolves it automatically (with no human resolver recorded). One open monitor incident at a time; duplicate failures fold into it. A declared incident sits in its own slot, so one left open does not stop the writer opening, paging and counting a real outage under it.

"Sustains" is two gates, not one. A region counts as failing only after `alert_confirmations` failures in a row from that region alone (default 2), and then the monitor's region policy decides how many failing regions have to agree before the incident opens. The default is majority, meaning more than half of the regions currently reporting results for that monitor; a region that stops reporting drops out of the vote instead of counting as down. Under that default, a monitor probed from several regions does not open an incident because one location had a bad minute. See [Multi-region probes](multi-region.md) for the policy list, or [Probe regions](hosted/regions.md) on the hosted service.

Visibility is derived at open time: if the monitor is a component of an enabled status page the incident opens `public`, otherwise `internal`. A monitor on no page still gets a fully tracked internal incident.

You can also declare an incident by hand from the console (`/incidents/declare`) — for a problem a monitor can't see, like a customer report or a partner outage. A manual incident may stand alone or link to a monitor. A monitor holds at most one open declaration, independently of whatever the writer is doing, so a declaration you forget to close never silences detection on that monitor.

Declaring is quiet by default: the incident opens `internal` and pages nobody, so you can open one while you are still working out what broke. The form offers both louder options explicitly — publish it to the status pages carrying the linked monitor (or, with no monitor, the pages you pick), and alert the org's channels now. Over the API those are the `visibility` and `notify` fields on `POST /api/v1/incidents`, both off unless set. Alert mail for a declared incident says it was declared by hand, so nobody reads it as a monitor detection.

A declared incident does not dent the monitor's uptime unless you say it should. Uptime is measured from checks, and a declaration has no failing check behind it, so counting one by default would move a number nothing in the check history can explain. The declare form asks the question directly, and the incident stays listed either way with the monitor's incident list marking an excluded one `not counted`. Switch it on for a real outage the checks could not see, like payments failing while the site answers fine. Over the API this is `counts_as_downtime` on `POST /api/v1/incidents`, off unless set, and it can be flipped later with `PATCH /api/v1/incidents/{id}`. It also decides what the public page paints: see [Public status page](public-status.md). An incident a monitor opened always counts: its failed checks are the evidence, and the API refuses to move that with a `422`.

### A declaration and a detection on the same monitor

The two are independent objects and hold separate slots, so a monitor can carry one of each at once. Walking the usual sequence:

1. **You declare.** One `manual` incident opens, quiet unless you asked it to page, excluded from uptime unless you asked it to count. The writer ignores it from here on.
2. **The monitor then goes down for real.** The writer sees no monitor incident open for it, so it opens its own, pages on-call, and derives visibility as usual. Both incidents are now open. Only the writer's one moves uptime; yours still reads `not counted`.
3. **The monitor recovers.** The writer resolves the incident it opened, stamped at the recovery, and uptime is dented by exactly that window.

Your declaration is left alone in step 3. The writer never resolves what a person opened, so a declaration stays open until someone closes it, and while it is open the org keeps reporting an active incident. Resolve declarations when you are done with them.

Each incident carries a **severity** (`minor` / `major` / `critical`) and an **urgency** (`high` pages on-call, `low` notifies only; urgency decides how hard it pages once alerting is on). A declared incident takes the severity you choose; an auto-opened one currently defaults to `major` until an operator changes it.

## The console

`/incidents` is the operator console — a management surface distinct from the dashboard's at-a-glance banner. It lists incidents with severity, state, monitor, assignee, and age, filterable by state. `/incidents/{id}` is the detail view: header, the action bar, the trigger sample, and the activity log.

The action bar drives the lifecycle:

| Action | Effect |
|---|---|
| **Acknowledge** | `state = acknowledged`, records the first acker, stops escalation. Re-acking keeps the original acker and time. |
| **Resolve** | `state = resolved`, records the resolver. (A sustained recovery auto-resolves with no resolver.) |
| **Reopen** | A resolved incident returns to `triggered` and re-arms escalation. |
| **Assign / unassign** | Set or clear the owning responder. |
| **Add note** | Free-text entry on the internal timeline. |

Acknowledge and resolve prompt for an optional note so you can capture the *why* at the moment you act.

### The activity log

Every lifecycle action writes an append-only event to the incident's internal timeline. Each entry answers **who, when, and what**: the acting member's email (system-driven transitions show `system`; an action taken through the MCP server is badged `via MCP`; one taken from a push notification shows `notification`, since holding the alert is the only proof and there is no member to name), an exact timestamp, and any note. This is the audit trail — the foundation for tracking response is a healthy habit of leaving notes, and the log makes that habit visible.

## Paging and escalation

When an incident opens, the escalation engine pages the responsible channels. Paging reuses the same transports as regular notifications, every channel kind included: Slack, Discord, Teams, Google Chat, Mattermost, Telegram (one-tap linked or bring-your-own bot), WhatsApp, email, SMS, PagerDuty, ntfy, Pushover, Gotify, and webhooks (see [Notifications](notifications.md)). Pushover emergency-priority pages are receipt-tracked and cancelled on resolve. Telegram rate-limit responses are honoured: a 429 with `retry_after` pushes the retry out at least that far.

An **escalation policy** is an ordered ladder of levels. Each level waits a delay, then pages its targets; if no one acknowledges, the engine advances to the next level, and can repeat the ladder a configured number of times before giving up. Acknowledging the incident halts the walk.

A policy's targets can be:

- **a channel** — pages that notification channel directly;
- **a user** — pages the channels that member has chosen to be reached on (see on-call below);
- **a schedule** — resolves who is on call right now and pages them.

Policies are owner-managed at `/settings/escalation`: build the ladder, set per-level targets, and pick an org-default policy. Bind a specific policy to a monitor from the monitor's edit form. Resolution at page time is: the monitor's own policy, else the org default, else **simple mode** — the monitor's bound notification channels are paged directly, with no laddered re-paging.

> **One notification source.** Every down/up notification flows through the incident engine — there is no separate per-monitor alert dispatch, so a monitor can never double-page. The `escalation.enabled` switch gates only the policy machinery (ladder walk, policy UI); with it off, monitors still page their bound channels in simple mode.

A monitor inside an active maintenance window with `suppress_alerts` on pages nobody while the window runs. The incident opens and records normally and the timeline says paging was held; the page itself is held, not dropped, so when the window ends with the incident still open the release sweep pages the channels the hold never reached. An escalation ladder mid-walk parks rather than stopping, and picks up where it left off. An incident that was already paged before the window simply resumes its reminders afterwards — its backoff is left alone, since the on-call already knows about it. An incident an operator declares by hand always pages, window or not: they declared it during the window on purpose.

While an incident stays **unacknowledged**, the engine re-sends a reminder on the monitor's `renotify_interval_secs` cadence (default hourly, `0` disables), doubling the gap after each one up to a day; acknowledging or resolving stops both the reminders and any escalation walk. A reminder carries the `reminder` reason rather than a second `opened`, so it reads as a reminder in the delivery log and does not re-ping a Slack channel. Failed deliveries retry on exponential backoff and are dead-lettered after the attempt cap. Every attempt is auditable: the incident detail page has a **Delivery** section, and `GET /api/v1/incidents/{id}/notifications` returns the same log.

## On-call schedules

On-call schedules (owner-managed at `/settings/on-call`) decide *which human* a `user` or `schedule` target pages.

A schedule has a timezone and one or more **layers**. Higher layers win when stacked. Within a layer, participants rotate in listed order on a cadence:

| Rotation | Handoff |
|---|---|
| `daily` / `weekly` | Hands off at the same wall-clock time each period, in the schedule's timezone — stable across daylight-saving changes. |
| `custom` | A fixed number of seconds. |

**Overrides** cover a specific window with a chosen person (vacations, swaps) and beat the rotation while active. The editor's calendar builds one by clicking a start day, then an end day, then choosing who covers. A "who's on call now" widget resolves the current responder, and `GET /api/v1/on-call/who` answers it programmatically.

Resolution at page time, for a given instant: an override covering that instant wins; otherwise the highest layer that has participants, advanced by its rotation. The result is a set of users.

### Contact channels

A resolved user is paged through the org [notification channels](notifications.md) they have opted into — each member picks, on the on-call page, which notification channels reach them. A `user`/`schedule` target therefore resolves to people, then to their chosen channels; the paging log records the targeted user alongside the channel. If a member has chosen no channels, they resolve but cannot be paged.

## Publishing to a status page

Internal incidents never reach customers. Publishing is the explicit gate.

Every public read — the status page, its JSON API, the RSS feed, and the history markers — filters on `visibility = 'public'`, so an internal incident on a public-component monitor never leaks. Monitors that sit on an enabled status page open `public` automatically; everything else (manual incidents, monitors not on a page) stays internal until you publish, either from the incident detail page or from the declare form itself.

From the incident detail page, **publish** flips visibility to `public` (optionally seeding a public title) and **unpublish** hides it again. A published incident appears on any status page whose components include its monitor.

An incident with no monitor, like a network outage in one data centre that touches every customer, has no component to follow. You pick its pages instead, one by one or all at once, on the declare form or the incident page, and can change the list while it is live. Over the API that is `status_page_ids` on `POST /api/v1/incidents` and on `POST /api/v1/incidents/{id}/publish`. Publish replaces the whole list, so to add or drop one page, read the current list from `GET /api/v1/incidents/{id}` and send it back changed. Publishing one to no page at all is refused with `INCIDENT_STATUS_PAGE_REQUIRED` rather than reported as a success nobody can see, and naming pages for an incident that has a monitor is refused with `INCIDENT_STATUS_PAGES_WITH_MONITOR`, since the monitor already decides where it shows. On those pages it sets the headline from its severity, and its updates reach their subscribers. Narrate it for customers with public updates (the `investigating` → `monitoring` → `resolved` timeline); posting an update is separate from the internal state, exactly as the two-axis model intends.

## Postmortems

A resolved incident can carry one postmortem — a retrospective with a summary, root cause, impact, and a list of action items (each with optional owner and a done flag). Write it from the incident detail page (**write / edit postmortem**).

Publishing a postmortem surfaces it on the public incident page: customers see the summary, root cause, impact, and the action-item text and done state. Internal detail — the action-item *owner* — is never exposed publicly. A draft stays private until you publish, and publish/unpublish are recorded on the incident's activity timeline with the acting member, so the retrospective's own history is auditable.

## Metrics and reporting

`/incidents/reports` is a metrics dashboard over a trailing window (7 / 30 / 90 days):

- **MTTA** — mean time to acknowledge (`acknowledged_at − started_at`).
- **MTTR** — mean time to resolve (`ended_at − started_at`).
- Total incidents, counts by severity and by state, auto-resolved vs human-resolved, and the noisiest monitors.

The same numbers are available to automation through the MCP `get_incident_metrics` tool.

## MCP tools

An LLM connected through the [MCP server](mcp.md) can triage and operate incidents within its granted scopes: read the incident list (open ones, or resolved history in a window) and detail, read metrics, and — with write scope — acknowledge, resolve, publish or unpublish, and post public updates. Customer-supplied incident text is always returned as labelled data, never as instructions. See [MCP server](mcp.md) for the full tool table and scopes.

## Auth and scopes

| Surface | Requirement |
|---|---|
| Incident lifecycle (ack / assign / resolve / note / publish / declare) | `incidents:write` — any member; responders are not owners |
| Operator lifecycle actions in the org audit log | written for every member action (declare, acknowledge, resolve, reopen, publish, unpublish); automatic monitor transitions are not audited |
| Reading incidents and metrics | `incidents:read` |
| Escalation policies + on-call schedules (config) | `oncall:write` (owner-only); `oncall:read` to view |

There is no incident-delete: incidents are resolved, never deleted, to keep the audit trail intact. Owner and member are the only roles — any member can be assigned, put on a schedule, paged, and can operate an incident; owners manage the escalation/on-call configuration.

## Configuration

The `[escalation]` block (env prefix `UPTIMEPAGE_ESCALATION__*`) controls the engine:

| Key | Default | Purpose |
|---|---|---|
| `enabled` | `false` | Enable escalation policies (ladder walk + policy/on-call UI). Off, incidents still page the monitor's bound channels directly (simple mode). |
| `tick_interval_secs` | `15` | How often the engine sweeps for due escalations and failed-page retries. |
| `max_pages_per_tick` | `500` | Backpressure cap on pages re-sent per sweep. |
| `max_attempts` | `5` | Give up paging a channel after this many failed attempts. |
| `channel_failure_limit` | `3` | Flag a channel as not delivering after this many deliveries in a row exhaust `max_attempts`, and mail the org's owners at most once a day. The channel keeps being paged; nothing is turned off. Any send that lands clears the run, as does turning the channel off and back on. `0` never flags. |

Per-org limits (`max_escalation_policies`, `max_on_call_schedules`, `on_call_enabled`) are plan quotas; see [Quotas & rate limits](quotas.md).
