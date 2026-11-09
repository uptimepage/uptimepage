# Alert sources

An alert source lets a tool that already watches your systems page through Uptimepage. Prometheus Alertmanager, Grafana, Datadog, or a script you write sends its alerts to the source's URL. A firing alert opens an incident, the incident pages through the source's escalation policy or channels, and the tool's recovery closes it. Acknowledgements, escalation, reminders, flap damping and closing notices work exactly as they do for a monitor's incident.

Alert sources come with the Team plan, five per account, counted across the account's organizations. See [Quotas and limits](quotas.md).

## How it fits

A source is one URL, one tool and one route. Give each team's receiver, contact point or webhook a source of its own, and each source pages its own people. Every tool on the list already routes by team, so this covers real setups without a rule engine.

An incident an alert opened has `origin: alert` and no monitor. It does not count against any monitor's uptime and does not appear on a status page. If customers need to hear about it, publish it by hand like a declared incident.

## Setting one up

Open **Settings → Alert sources** and choose **+ source**. Pick the tool, name the source, and choose who it pages:

- its own escalation policy, or
- the org default policy, when the source has none of its own, or
- its channels, when no policy applies.

A source must reach someone. Saving one with no policy of its own, no org default policy and no enabled channel is refused with `422 ALERT_SOURCE_NO_ROUTE`. A route can still be lost later, when a policy is deleted or a channel disabled. The source then shows **pages nobody** in the list and on its page, and an alert still opens an incident but pages no one.

The source's page shows its URL and token, masked until you choose **show**, and the setup for its tool. Send the test alert from there: it opens a real incident and pages the route, so you see the whole path work once.

The token goes either in the path, `POST /hooks/alerts/{token}`, or in a header, `POST /hooks/alerts` with `Authorization: Bearer {token}`. Prefer the header where the tool supports one, so the token stays out of URLs the tool shows in plain text.

## Prometheus Alertmanager

Add a receiver and route the alerts this source should page to it:

```yaml
receivers:
  - name: uptimepage
    webhook_configs:
      - url: https://app.uptimepage.dev/hooks/alerts
        send_resolved: true
        http_config:
          authorization:
            credentials: <token>
```

Keep `send_resolved: true`, or nothing ever closes. Leave `max_alerts` at 0: Alertmanager computes a group's status before cutting the list, so a cut payload can read `resolved` while a cut alert still fires. Uptimepage never closes an incident on such a payload, which leaves it open until the next full one or a person.

By default one Alertmanager group is one incident, keyed on its `groupKey`. The group key contains the route path, so editing the routing tree while a group is firing leaves that incident without its recovery, and a person resolves it. Alertmanager's own PagerDuty and Opsgenie receivers share this trap.

## Grafana

Create a contact point of type **Webhook**:

| Field | Value |
|---|---|
| URL | `https://app.uptimepage.dev/hooks/alerts` |
| HTTP method | `POST` |
| Authorization header | scheme `Bearer`, credentials the source's token |

Then point the notification policies whose alerts this source should page at the contact point.

Grafana stores a webhook URL in plain text, so a token in the path is visible to anyone who can read the contact point. A signing secret closes that gap: set one on the source (16 to 256 characters) and the same secret in the contact point's **HMAC signature** field, with `X-Grafana-Alerting-Signature` as the signature header and `X-Grafana-Alerting-Timestamp` as the timestamp header. A signed source refuses any request without a valid signature over the whole body (`401 ALERT_SIGNATURE_INVALID`), and a timestamp more than five minutes off. Use the contact point's own test button to try it, since an unsigned test command is refused.

## Datadog

Under **Integrations → Webhooks**, add a webhook named `uptimepage` with the source's full URL and this payload:

```json
{
  "transition": "$ALERT_TRANSITION",
  "cycle_key": "$ALERT_CYCLE_KEY",
  "monitor_id": "$ALERT_ID",
  "scope": "$ALERT_SCOPE",
  "title": "$EVENT_TITLE",
  "message": "$TEXT_ONLY_MSG",
  "priority": "$ALERT_PRIORITY",
  "link": "$LINK",
  "tags": "$TAGS",
  "date": "$DATE"
}
```

Mention `@webhook-uptimepage` in the message of every Datadog monitor that should page. `Triggered`, `Re-Triggered`, `Renotify`, `Warn`, `Re-Warn`, `No Data` and `Re-No Data` fire; `Recovered` closes; anything else is logged and ignored. An alert is keyed on `$ALERT_CYCLE_KEY`, or on the monitor id and scope when that is empty.

## Scripts and other tools

A source of kind **generic** reads a small subset of the PagerDuty Events v2 shape, so anyone who has written that integration already knows it:

```json
{
  "event_action": "trigger",
  "dedup_key": "checkout-5xx",
  "payload": {
    "summary": "Checkout error rate above 5%",
    "source": "billing-worker",
    "severity": "critical",
    "timestamp": "2026-11-06T09:14:00Z",
    "custom_details": { "region": "eu-west-1" }
  },
  "links": [{ "href": "https://grafana.example.com/d/abc", "text": "Dashboard" }]
}
```

`event_action` is `trigger` or `resolve`, and `dedup_key` ties the two together; it is required. `severity` is `critical`, `error`, `warning` or `info`, and defaults to `error`. Send `payload.timestamp` (RFC 3339) whenever you can: without one, events are applied in the order they arrive, and an old resolve cannot be told from a new one. Unknown keys are ignored. `acknowledge` is not accepted.

```sh
curl -sS -X POST https://app.uptimepage.dev/hooks/alerts \
  -H "Authorization: Bearer $UPTIMEPAGE_ALERT_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"event_action":"trigger","dedup_key":"backup-nightly","payload":{"summary":"Nightly backup failed","severity":"error"}}'
```

Any tool that can template its webhook body can use this shape too.

## What an alert becomes

### One incident per group, or per alert

| Tool | Grouping **group** (default) | Grouping **alert** |
|---|---|---|
| Alertmanager, Grafana | the group's `groupKey` | each alert's `fingerprint` |
| Datadog | `cycle_key`, else monitor id and scope | the same |
| Generic | `dedup_key` | the same |

Grouping by group keeps the tool's own grouping, which is what its users already tuned: a 200-pod failure in one group is one incident. Per alert, any label that changes, such as a pod name, makes a new incident. Grouping can change only while the source holds no open incident (`409 ALERT_SOURCE_GROUPING_LOCKED`), because the change moves every key. The tool itself is fixed when the source is created.

### Firing, repeats and recovery

- **A firing opens** an incident titled from the alert's summary, or its name, and pages the route.
- **A repeat pages nobody.** It updates the firing count and the last event time. Reminders come from the source's own reminder setting, whatever the tool's resend interval. A repeat never clears an acknowledgement.
- **Worse severity** raises the incident's severity with one line on its timeline, and pages nobody. A person who lowered the severity is not overruled by the tool repeating itself.
- **A recovery closes** the incident, and everyone paged hears it ended. In a group, the incident closes only when the whole group has recovered.
- **An old recovery** never closes a newer incident: a recovery dated well before the incident fired, or matching one already applied, is logged and ignored. An old firing is never dropped, since a missed page costs more than an extra one.
- **A person resolves while the tool still fires**: the tool's next repeat opens a fresh incident, which pages. The tool still says it is broken.
- **Flapping** uses the same damper as monitors: an alert that opens five times inside an hour pages with a flapping note, and later opens page only if they last.

### Severity

Alertmanager and Grafana read the `severity` label: `critical`, `page` or `p1` is critical; `error`, `high`, `major` or `p2` is major; any other value is minor; no label is major. In a group, the worst firing alert counts. Datadog reads the priority: P1 critical, P2 major, P3 to P5 minor, none major, and a `Warn` or `No Data` transition is minor. A minor incident still pages, at low urgency.

### When too much arrives

A source holds at most 50 open incidents. Past that a new alert opens nothing; instead one summary incident opens and pages once ("Prod Grafana hit its limit of 50 open alerts"), counting what was refused, and closes when the source is back under the limit. A refused alert gets its chance at the tool's next repeat, and the delivery log lists its key.

A body Uptimepage cannot read, sent with a valid token, opens one incident ("Prod Datadog sent an alert we could not read") and is kept in the delivery log, because a tool tried to page someone. Later unreadable bodies fold into it. A person resolves it.

## The delivery log

Each source's page lists every request it received, newest first, one row per event: when it arrived, its outcome, its key, and the incident it opened or touched with that incident's state. Expand a row for the reason and the start of the request's body. Filter by outcome:

| Outcome | Meaning |
|---|---|
| `opened` | opened an incident |
| `repeat` | an incident was already open for the key |
| `resolved` | closed the key's incident |
| `ignored` | changed nothing: a recovery with nothing open, a stale recovery, a transition Uptimepage does not act on |
| `paused`, `held` | arrived while the source was paused or held by the plan |
| `capped` | refused at the 50 open incidents limit |
| `unparsed` | could not be read |
| `unavailable` | the database failed or could not keep up; the tool was told to retry |

The last five together are **not applied**, the count the source's page and the list show for the last 24 hours. Bodies are kept for the plan's evidence window, rows for its retention. A request that only repeats alerts already open, and changes nothing about them, keeps no body.

## Responses and limits

| Case | Status |
|---|---|
| Accepted: opened, repeat, resolved, ignored, paused, capped, unparsed | `202` |
| Unknown token, or a source deleted mid-request | `404 ALERT_TOKEN_UNKNOWN` |
| Bad or missing signature on a signed source | `401 ALERT_SIGNATURE_INVALID` |
| A firing for a new alert on a source the plan holds | `403 ALERT_SOURCE_HELD` |
| Body over 8 MiB | `413 ALERT_BODY_TOO_LARGE` |
| Body not finished in 10 seconds | `503 ALERT_BODY_STALLED` with `Retry-After` |
| Rate limited, or the database unavailable | `503` with `Retry-After` |

Throttling answers `503`, never `429`: Alertmanager and Datadog retry server errors only, so a `429` would turn a slow moment into a lost page. Every event counts against the account's `alert_events_per_minute` budget, and each token gets a burst of 300 requests, then 10 a second.

A body over 8 MiB pages nobody. On the hosted service the edge refuses it before the app sees it, so the delivery log has no entry for it and the sender's own log is the only trace. Eight megabytes holds thousands of alerts in one group, so this means a misconfigured sender.

## Pausing, holds, rotation and deletion

**Pausing** a source keeps it listening. A firing opens nothing, a recovery still closes, and the tool gets a `202` so it stops retrying. Nothing is queued: an alert still firing when you resume opens its incident at the tool's next repeat.

A page that failed before the source was paused keeps retrying on its backoff after the pause, as a monitor's does. New firings stay quiet and the escalation ladder holds its steps.

**Holds.** On a plan without alert sources, sources are held: a firing for a new alert is refused with `403 ALERT_SOURCE_HELD`, and alerts already open still close. The newest are held first. A held source comes back with the same URL when the plan covers it again.

**Rotating** the token mints a new one at once. The old one keeps working for 24 hours, so the tool still pages while you update it, unless you cut it off early.

**Deleting** a source closes the incidents it held open, without counting them as resolved. Its URL stops working at once.

## Watching the watcher

If Alertmanager dies, nothing fires, and nothing pages. Cover that with a [heartbeat monitor](monitor-types.md#heartbeat): route Alertmanager's always-firing `Watchdog` alert (kube-prometheus ships one) to the heartbeat's ping URL, which accepts a `POST` with any body.

```yaml
route:
  routes:
    - matchers: [alertname = "Watchdog"]
      receiver: watchdog
      group_interval: 1m
      repeat_interval: 1m
receivers:
  - name: watchdog
    webhook_configs:
      - url: https://app.uptimepage.dev/ping/<heartbeat-token>
```

Give the heartbeat a period of a few minutes. When the pings stop, Alertmanager or the path to it is down, and the heartbeat's own incident pages.

## API, Terraform and MCP

Sources are managed over [`/api/v1/alert-sources`](api.md#operator-endpoints-alert-sources), or with the Terraform provider's [`uptimepage_alert_source`](terraform.md#alert-sources), with the `alert_sources:read`, `alert_sources:write` and `alert_sources:delete` scopes. The URL and token come back only with `alert_sources:write`. Incidents carry `origin`, the source and what the tool sent; `GET /api/v1/incidents?source={id}` lists one source's incidents, a deleted source's included.

The [MCP server](mcp.md) reads sources with `list_alert_sources`, never with their URL or token, and shows what a tool sent on `get_incident`. There is no tool to create one: a source is one-time setup, and its token does not belong in a transcript.
