# Multi-region probes

Run checks from more than one location and keep every result attributed to the region that produced it. A single **control plane** owns all state (Postgres, ClickHouse, the web UI, alerting, and a scheduler for its own region); additional boxes run as stateless **agents** that pull their region's monitor config and ship results back.

This is opt-in. A default deployment is a single region — the control plane checks everything itself and nothing below changes.

## Model

- **Control plane** — one process holding Postgres + ClickHouse + the web UI + alerting + a scheduler. Its own region is a normal region row identified by `scheduler.region` (default `"default"`); rename it to a real location, it is not a sentinel.
- **Agent** — a process started with `[agent] enabled = true`. It runs no database, web UI, or alerting. It pulls its region's decrypted monitor config from the control plane over authenticated HTTPS, runs the checks locally, and POSTs results back to the central ingest API. Agents never touch ClickHouse or fire alerts.
- **Region is the partition key.** One agent per region needs no coordination — there is no leader election. (Running more than one agent in the same region, or more than one control plane, is out of scope for this version.)

New targets are assigned to every region flagged `default_selected` (all of them until an operator opts one out), capped at the plan's `max_regions`, unless the create names its own `regions`. An opted-out region stays fully pickable on the monitor form, it just starts unchecked. That is how to offer a vantage point with a known-bad network path without putting it on every new monitor. If nothing is flagged, targets fall back to `scheduler.default_region` (empty falls back to `scheduler.region`). At boot the control plane reconciles the configured region rows and backfills any unassigned target to the default region, so enabling regions never leaves a target unchecked.

## Running an agent

On the agent box, point at the control plane and name the region. The token carries the agent's capability — supply it by environment variable, never in a committed file:

```toml
[agent]
enabled = true
control_plane_url = "https://app.example.com"
region = "eu-west"
pull_interval_secs = 30
flush_interval_secs = 5
buffer_capacity = 10000
```

```bash
UPTIMEPAGE_AGENT__TOKEN=sm_agent_…   # the token minted by POST /operator/agents
```

Ping (ICMP) checks open an unprivileged `SOCK_DGRAM` ICMP socket on the agent. Docker grants this by default (`net.ipv4.ping_group_range` is pre-widened in containers); on bare hosts widen the sysctl to cover the agent's GID or grant the binary `CAP_NET_RAW`. An agent without either reports ping checks as `error` with the reason — every other check kind is unaffected.

Heartbeat monitors never reach agents: they are passive (customer systems ping the control plane, which evaluates the ping age in memory), so the config-pull and dispatch surfaces exclude the kind entirely.

Flow monitors run a headless browser, so they execute only on agents that ship a browser engine (the flow-capable set that agents self-report). A flow monitor's assigned regions are clamped to that set when it is saved, and an agent without the engine never receives it in its config pull.

The agent must reference a region and a token that already exist (see the operator surface below). Pull and ingest behaviour:

- **Pull** (`GET /api/agent/targets`) — `401`/`403` is terminal: the agent clears its cached config and pauses, so revoking or disabling the agent stops the probe. `5xx`/timeout is transient: it keeps serving the last-known config. Responses are content-hashed with an ETag, so a credential re-encrypt invalidates the cache even without a config change.
- **Ingest** (`POST /api/agent/results`) — region and agent id are taken from the token, never trusted from the body. Rows that are clock-skewed or belong to a region the agent isn't assigned are dropped per-row (the rest of the batch still lands) and counted, rather than rejecting the whole batch. Cross-process de-duplication is authoritative in ClickHouse; a re-sent identical batch is idempotent.
- **Dispatch** (`GET /api/agent/dispatch`, `POST /api/agent/dispatch/results`) — a long-poll surface for on-demand runs ("check now" and channel tests), so a manual check reaches the agent in the right region instead of running centrally.

## Operator surface

Regions and agents are managed instance-wide (across all tenants) under `/operator/*`, gated by a static bearer secret. Set it by environment variable; an empty value disables the surface entirely (it `404`s, so it is invisible when off):

```bash
UPTIMEPAGE_OPERATOR__ADMIN_TOKEN=…
```

```
Authorization: Bearer <that-secret>
```

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/operator/regions` | list regions |
| `POST` | `/operator/regions` | create a region (`id` is a `[a-z0-9-]` slug, `name`, optional geo fields: `city`, `country_code`, `continent`, `latitude`/`longitude`) |
| `PATCH` | `/operator/regions/{id}` | rename / relocate, enable / disable a region (`enabled`), or opt it in / out of new monitors' defaults (`default_selected`) |
| `DELETE` | `/operator/regions/{id}` | delete a region — `409` while it still holds agents or assigned targets |
| `GET` | `/operator/agents` | list agents |
| `POST` | `/operator/agents` | mint an agent — the response carries its `sm_agent_…` token **once** |
| `PATCH` | `/operator/agents/{id}` | rename / enable / disable an agent |
| `DELETE` | `/operator/agents/{id}` | delete an agent |

The agent token is shown only at creation; store it when it is minted. Disabling an agent is immediately enforced on its next pull. There is no token-rotation endpoint yet — rotate by deleting and re-creating the agent.

Disabling a **region** stops it being scheduled and stops config-pull for it (its agents receive no targets) while keeping its stored history — a reversible alternative to deleting, which the foreign keys block while the region is in use.

A typical bring-up: create the region, mint an agent in it, copy the token to the agent box's `UPTIMEPAGE_AGENT__TOKEN`, start the agent.

## Viewing per-region data

Once results carry a region, the operator surfaces let you slice by it:

- **Dashboard** — a `region:` filter in the subhead (shown only when the org spans more than one region) scopes every fleet metric to one region. `?region=` is reflected in the URL.
- **Monitor detail** — a region selector scopes the KPI cards, latency and breakdown charts, and recent results. In the all-regions view the latency chart overlays one p95 line per region, and a **by region** table summarises uptime, p50, p95, and last status per region. Pick a region to drill into a single line.
- **REST API** — `/api/v1/targets/{id}/results`, `/latency`, and `/uptime` accept an optional `region=` query parameter; `/api/v1/targets/{id}/latency/by-region` returns one series per region. `GET /api/v1/regions` lists the enabled region catalog, `POST /api/v1/targets` takes an optional `regions` array, and `GET`/`PUT /api/v1/targets/{id}/regions` read and set a monitor's assignment — all under `targets:read`/`targets:write`. See [REST API](api.md#latency-series).

What deliberately **blends** across regions: the public status page's component status (the public "is it up" answer is region-agnostic by design), the monitors list, and incident timelines. Those combine every region under the monitor's region policy so a viewer sees one verdict, described under [Incident detection across regions](#incident-detection-across-regions).

## Incident detection across regions

Detection evaluates each region's recent run **independently** and then combines the verdicts, so one region's transient network blip can't corrupt the picture for a target probed from several places. There is always exactly one incident per target — its `region` is unset.

How the per-region verdicts combine is a **per-monitor policy**, set on the monitor form (default **majority**):

- **any** — open as soon as a single region is sustained-unhealthy.
- **majority** — open once more than half the reporting regions agree it's down (the standard defence against a single-location false positive).
- **all** — open only when every region is down.
- **count: N** — open once at least *N* regions are down. A count wider than the regions the monitor is assigned is clamped to the regions that report, so it can never be impossible to reach.

A monitor probed from a single region behaves the same under every policy.

The same policy decides the status you see, not only whether an incident opens. A monitor whose failing regions have not reached its quorum shows **degraded**, not down: something is wrong somewhere, but not enough regions agree to call it an outage. Reach quorum and it shows the worst failing status. This is why the monitors list, the dashboard and the MCP tools can read degraded while the incidents tab stays empty, and it is the intended pairing rather than a disagreement. Filter the console to a single region and you get that region's raw verdict instead, since a region view is asking what that probe saw.

Displayed status folds each region's most recent result from the last 24 hours, so it can turn degraded a check or two before an incident opens: the incident writer additionally waits for `alert_confirmations` failures in a row from a region, and the folded status does not. A region that stops probing a monitor drops out of the fold within that day. Monitor detail is the exception: its badge folds the by-region table under it, over the range that page is showing, so the two always agree with each other.

The policy counts regions that delivered results inside the evaluation window, not regions merely assigned. Agents push their results to the control plane, so an agent that goes quiet takes its region out of the vote: no results means no down vote and no recovery evidence. A silent region can neither open nor close an incident, and the threshold recomputes over the regions still reporting.

See [Configuration](configuration.md) for the `[scheduler]`, `[agent]`, and `[operator]` keys, and [Architecture](architecture.md) for where the pieces sit.
