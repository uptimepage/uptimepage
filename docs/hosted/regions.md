# Probe regions

This page covers the hosted service at `uptimepage.dev`. If you run your own instance, you choose your own regions and add agents yourself: see [Multi-region probes](../multi-region.md).

## Where we check from

| Region | Location |
|---|---|
| `us-west` | San Jose, United States |
| `us-east` | Secaucus, United States (New York metro) |
| `eu-frankfurt` | Frankfurt, Germany |
| `eu-helsinki` | Helsinki, Finland |
| `apac-sg` | Singapore |

A new monitor starts in a default set of regions rather than all of them. Today the defaults are New York, Frankfurt and Helsinki; San Jose and Singapore are opt-in, one checkbox away on the monitor form. On Standard, which caps a monitor at three regions, adding one means unticking another.

More regions are rolling out. Existing monitors pick them up as soon as you assign them, with no migration and no change to your history.

## Assigning regions to a monitor

A monitor runs in every region you assign it. Each result is stored with the region that produced it, so the monitor detail page can show per-region latency and you can filter any chart down to one region.

Standard is capped at three regions per monitor. Founding and the paid plans have no cap, so they pick up new regions as they open.

## How a failure is decided

Each monitor carries a region policy that says how many regions have to agree before it counts as down:

- `any` — one failing region is enough
- `majority` — the default, more than half of the regions currently reporting results
- `all` — every assigned region
- a count, for example `{"count": 2}`

`majority` is the default because it absorbs the common case of one probe path having a bad minute without waking anybody. Use `any` when you want the earliest possible signal and can tolerate more noise, and `all` for a monitor that is only meaningful when it is globally unreachable.

Region agreement and the consecutive-failure threshold are two separate gates, applied in that order. A region counts as failing only after `alert_confirmations` failures in a row from that region alone (default 2), and then the region policy decides whether enough failing regions have agreed to open an incident. Recovery works the same way in reverse: a failing region stops counting once it has a matching run of passing checks, and the incident closes when the regions still counting drop below the policy, held for the monitor's recovery period when it has one. The recovery has to come from a region that was failing: one that never failed is no evidence on its own.

Regions push their results to the control plane, and the policy counts the regions that are actually reporting. A region that stops reporting, for example because it lost connectivity, drops out of the vote entirely: it is not counted as down, and the policy applies to the regions still delivering results. Missing data alone never opens an incident and never closes one: a failing region that goes quiet mid-outage keeps the incident open until a region that was failing passes again. Only when every failing region has gone quiet do the regions still reporting decide: each of them has to be confirmed passing, and the incident ends when the last of them was, held for the recovery period from then.

## What our probes cannot reach

Some endpoints refuse traffic from datacentre address ranges, which is where every one of our regions lives. When that happens the monitor fails with `connect timeout`, or sometimes `network unreachable`, from every region at once, while the same URL loads fine when you open it yourself. Nothing is down. The host is declining to talk to us.

The clearest case is mainland China. Endpoints served from inside China are routinely unreachable or lossy from foreign networks, so a monitor pointed at one will show a failure rate in the tens of percent, scattered through the day, with no pattern you can act on. We have no probe inside mainland China and no plan to add one, because operating there requires a licence we do not hold. If you need that coverage, we are the wrong tool for it and we would rather say so here than let you find out over a day of bad data.

The same thing shows up in smaller ways elsewhere: geo-fenced APIs, aggressive bot protection in front of a login page, and firewalls that drop anything not coming from a residential range.

How to tell this apart from real downtime:

- For an HTTP response that matches a supported CDN/WAF signature, the result names the access-policy diagnosis separately from the authoritative status error. An incident names a provider only when the same diagnosis meets the monitor's region quorum, and, when more than one region confirmed the failure, reports how many of them agree.
- Run **check now** and compare regions. A block from our side usually fails in every region at once, where a real outage often starts in one region and spreads.
- Look at connect time on the checks that did succeed. If they connect in a couple of hundred milliseconds and the failures are hard timeouts, the path is being dropped rather than being slow.
- Open the URL yourself, from a connection that is not a datacentre. If it answers there and times out from every region you assigned, the difference is who is asking, not whether the service is up.

For an HTTP policy block, the durable fix is a small health endpoint that is exempt from browser challenges and authenticated with a secret request header. Uptimepage can source that header from an [org secret](../variables.md), so the endpoint does not need to be open to everyone. A narrowly scoped WAF rule matching both the health path and the header is safer than weakening protection for the public site. For a network that refuses datacentre traffic before HTTP, point the monitor at an endpoint that will answer us or switch to a heartbeat monitor and have the system itself ping us.

## Practical notes

Checks from different regions leave from different addresses, so an IP allowlist needs every assigned region. Hosted egress addresses are not guaranteed stable in every region: do not build a production rule from addresses you observe. Write to <hello@uptimepage.dev> before relying on IP-based access. Prefer the authenticated health-path pattern above when your edge supports it.

Browser flow monitors run only in regions that ship a browser engine. A flow monitor's region set is narrowed to those when you save it, so it never silently sits unassigned.

Heartbeat monitors never use regions at all. They are passive: your system pings us, and we evaluate the gap centrally.
