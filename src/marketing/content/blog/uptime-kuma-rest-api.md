+++
title = "Uptime Kuma API: what exists, and what to use instead"
date = "2026-09-05"
slug = "uptime-kuma-rest-api"
excerpt = "Uptime Kuma ships no REST API for managing monitors. Here is every HTTP route it exposes, what the Socket.IO interface really is, and where the wrappers stop."
tags = ["uptime kuma", "api", "monitoring as code", "open-source"]
draft = false
cta_label = "Manage monitors over a real API"

[[faqs]]
q = "Does Uptime Kuma have a REST API?"
a = "Uptime Kuma has no REST API for managing monitors. The HTTP routes it does expose are read-only badges, the public status page JSON, the push endpoint for heartbeat monitors, and a Prometheus metrics endpoint. Creating, editing, pausing and deleting monitors all happen over Socket.IO instead."

[[faqs]]
q = "How do I create a monitor in Uptime Kuma from a script?"
a = "You open a Socket.IO connection, log in with the admin username and password, and emit the add event with a monitor object. There is no documented schema for that object, so in practice you copy what the web UI sends, or you use a community wrapper that already did."

[[faqs]]
q = "What can an Uptime Kuma API key do?"
a = "An API key authenticates exactly one endpoint, the Prometheus metrics at /metrics. It cannot create, edit, pause or delete a monitor, and it is not a general access token for the rest of the app."

[[faqs]]
q = "Is the Python uptime-kuma-api library still maintained?"
a = "The original Python wrapper has had no commit since September 2023, and its own compatibility table stops at Uptime Kuma 1.23.2. Version 2 is the current line, so the ecosystem has split into several small forks rather than one maintained package."

[[faqs]]
q = "What is the safest way to automate Uptime Kuma today?"
a = "Push monitors are the one automation surface Uptime Kuma supports on purpose. Your job calls a token URL when it finishes, which covers cron and batch work without driving the private Socket.IO interface at all."
+++

> **TL;DR**
>
> Uptime Kuma will not let you create a monitor over HTTP. Its entire HTTP surface is status badges, public status page data, one push endpoint and a Prometheus scrape. The dashboard drives everything else over Socket.IO, a private interface the project never promised to keep stable. Every library billed as an Uptime Kuma API is a client for that interface.

You want fifty monitors created from a CSV. Or a check added in the same pull request that adds the service. Or a deploy script that pauses one monitor while it runs, then puts it back. So you go looking for the API docs and you cannot find them.

They do not exist. So this is the surface as it stands in [version 2.5.3](https://github.com/louislam/uptime-kuma/releases/tag/2.5.3), read out of the source on 5 September 2026.

## Every HTTP route Uptime Kuma serves

Two Express routers carry almost all of it, plus a few routes registered straight on the app. Everything not in this table serves the single-page app itself, including the status page HTML at `/status/:slug`, or else it is uploaded images, robots.txt, a change-password well-known link, and the first-run database setup page.

| Route | What it does | Auth |
|---|---|---|
| `/api/push/:pushToken` | Records a heartbeat for a push monitor | The token in the URL |
| `/metrics` | Prometheus metrics for all monitors | HTTP Basic |
| `/api/badge/:id/status` | SVG status badge | None |
| `/api/badge/:id/uptime/:duration?` | SVG uptime badge | None |
| `/api/badge/:id/ping/:duration?` | SVG response time badge | None |
| `/api/badge/:id/avg-response/:duration?` | SVG average response badge | None |
| `/api/badge/:id/cert-exp` | SVG certificate expiry badge | None |
| `/api/badge/:id/response` | SVG last response badge | None |
| `/api/status-page/:slug` | Public status page as JSON | None |
| `/api/status-page/heartbeat/:slug` | Heartbeat history for that page | None |
| `/api/status-page/:slug/incident-history` | Past incidents on that page | None |
| `/api/status-page/:slug/badge` | SVG badge for a whole status page | None |
| `/api/status-page/:slug/manifest.json` | Web app manifest for that page | None |
| `/status/:slug/rss` | RSS feed for that status page | None |
| `/api/entry-page` | Which page to show at the root | None |

Read the auth column again. One route writes anything, and what it writes is a single heartbeat. Nothing here creates a monitor, and nothing here will even list your monitors unless you already published them on a public status page.

## The API key does less than the name suggests

Uptime Kuma has an API key feature, in Settings, with expiry dates and an enable toggle. It is easy to assume that is the missing REST credential.

The key works on exactly one route, the Prometheus scrape at `/metrics`. In the source, `apiAuth` is attached there and nowhere else, so the key cannot touch a monitor at all.

Two things to know before you build on it. If API keys are switched off, that same endpoint falls back to HTTP Basic with your admin username and password, so a scrape config carries either a key or the login to the whole instance. And the key path is rate limited to 60 requests a minute across the instance, with the login path at 20.

## The real interface is Socket.IO

Everything the web UI does, it does by emitting Socket.IO events at the server. The monitor events are what you would want from a REST API, in the wrong shape:

```
add             editMonitor      deleteMonitor
pauseMonitor    resumeMonitor    getMonitorList
getMonitor      getMonitorBeats  getTags
```

Creating a monitor is an event literally named `add`.

To reach any of them you log in first, as the admin, over that same socket. The login event hands back a JWT you can reuse, and if the account has two-factor auth switched on, your script also has to produce a TOTP code from the shared secret. Then you emit `add` with a monitor object whose shape is written down nowhere, so you either copy what the browser sends or lean on somebody's wrapper that already did.

That matters for three reasons. The credential is the whole instance: there is no read-only token and no per-monitor scope, so a deploy script that pauses one monitor is holding the login that could delete all of them and change the admin password. The payload shape is not a contract, it is whatever the current UI happens to send, so a field can be renamed in a minor release and the project has broken nothing, because nothing outside the UI was meant to depend on it. And none of it is documented, so you learn it by reading [`server/server.js`](https://github.com/louislam/uptime-kuma/blob/master/server/server.js) or by reading someone else's wrapper.

## This is on purpose, and the project says so

It is worth being fair to Uptime Kuma here. This is not neglect. The [README lists the project's motivations](https://github.com/louislam/uptime-kuma#motivation), and one of them is:

> Try to use WebSocket with SPA instead of a REST API.

Uptime Kuma set out to be a single-page app with a live socket behind it, which is a good fit for a dashboard you keep open all day. A REST API was never the goal, so asking for one is asking for a different product.

The request is not being ignored either. [Issue 118](https://github.com/louislam/uptime-kuma/issues/118), "API functionality", has been open since July 2021 with 776 reactions and 77 comments, which makes it the most-reacted open issue in the repository. The maintainer went further than acknowledging it. In October 2023 he opened a pull request against that issue, [number 3854](https://github.com/louislam/uptime-kuma/pull/3854), called "Document the Socket.io API and try to convert it to a HTTP request". It is still a draft, and both it and the issue were open when this was written.

## The wrapper ecosystem, with dates

Every library called an Uptime Kuma API is a Socket.IO client wearing a nicer coat. Star counts and commit dates below come from the GitHub API on 5 September 2026, not from the project pages, because a repository can look alive off a dependabot branch while nobody has touched the code in two years.

| Project | Stars | Last commit | Notes |
|---|---|---|---|
| [`lucasheld/uptime-kuma-api`](https://github.com/lucasheld/uptime-kuma-api) (Python) | 394 | 2023-09-26 | The canonical one. Compatibility table stops at Kuma 1.23.2 |
| [`lucasheld/ansible-uptime-kuma`](https://github.com/lucasheld/ansible-uptime-kuma) | 188 | 2023-09-26 | Ansible modules built on the above |
| [`MedAziz11/Uptime-Kuma-Web-API`](https://github.com/MedAziz11/Uptime-Kuma-Web-API) | 279 | 2023-08-10 | A FastAPI bridge that gives you a REST facade over the socket |
| [`breml/go-uptime-kuma-client`](https://github.com/breml/go-uptime-kuma-client) | 16 | 2026-08-30 | Go client |
| [`pablofmorales/kuma-cli`](https://github.com/pablofmorales/kuma-cli) | 14 | 2026-04-02 | Command line client |
| [`exaland/uptime-kuma-api-v2`](https://github.com/exaland/uptime-kuma-api-v2) | 11 | 2026-08-23 | Fork of the Python wrapper for Kuma 2 |
| [`pbarone/uptime-kuma-api2`](https://github.com/pbarone/uptime-kuma-api2) | 11 | 2026-08-15 | Another fork of the same wrapper for Kuma 2 |

So the client almost everyone links to predates Uptime Kuma 2 entirely, and the forks that do cover version 2 are tiny. Two of them are the same idea, started a year apart by different people, with eleven stars each. That is what happens when the protocol underneath is neither stable nor written down: people rebuild the same wrapper instead of maintaining one together.

## What you can do today

If you are staying on Uptime Kuma, roughly in order of how much sleep it will cost you.

Push monitors are the one automation surface the project supports deliberately. Your job calls a token URL when it finishes, and the endpoint takes `status=up` or `status=down` plus optional `msg` and `ping` parameters. That covers cron and batch work without going near the socket. Same idea as [heartbeat checks for cron jobs](/cron-job-monitoring), and the reason behind [why cron jobs fail silently](/blog/cron-jobs-fail-silently).

To read state, scrape `/metrics` with an API key and query it there. That is the supported read path and it is stable. If the data is public anyway, `/api/status-page/heartbeat/:slug` returns real heartbeats with no credentials, limited to the monitors you chose to publish.

A socket wrapper is fine for work you are watching, like a one-off import of fifty monitors that you run once and check by hand. Pin the wrapper version, pin your Kuma version, and keep it out of any pipeline nobody is looking at.

Writing to the database directly is the one to avoid. People do it anyway. Kuma holds state in memory and writes heartbeats continuously, so a write behind its back is a corrupted instance waiting for the next restart.

## If you need monitoring as code

This is the point where the tool and the job stop matching. If your monitors have to live in Git, get reviewed in a pull request, and be created by a service account that cannot also delete them, you are asking a dashboard-first tool to behave like an API-first one.

That gap is what we built [Uptimepage](/vs/uptime-kuma) around. Monitors are defined in a [Terraform provider](/terraform-uptime-monitoring) or created over a [documented REST API](/docs/api) with scoped tokens, so a CI job can add a check without holding the keys to the account. There is an [MCP server](/mcp-server) too, if the thing creating monitors is an agent rather than a pipeline. The [side by side comparison](/compare/terraform-uptime-kuma) is the short version.

Kuma is still the right answer for plenty of homelabs and small teams, and our [roundup of self-hosted uptime monitors](/blog/best-self-hosted-uptime-monitoring-tools) says so. This is one specific edge, and it is the one most people hit second, right after the single shared login.

## Common questions

<details class="mk-faq">
<summary>Does Uptime Kuma have a REST API?</summary>
<div class="mk-faq__body">

Uptime Kuma has no REST API for managing monitors. The HTTP routes it does expose are read-only badges, the public status page JSON, the push endpoint for heartbeat monitors, and a Prometheus metrics endpoint. Creating, editing, pausing and deleting monitors all happen over Socket.IO instead.

</div>
</details>

<details class="mk-faq">
<summary>How do I create a monitor in Uptime Kuma from a script?</summary>
<div class="mk-faq__body">

You open a Socket.IO connection, log in with the admin username and password, and emit the add event with a monitor object. There is no documented schema for that object, so in practice you copy what the web UI sends, or you use a community wrapper that already did.

</div>
</details>

<details class="mk-faq">
<summary>What can an Uptime Kuma API key do?</summary>
<div class="mk-faq__body">

An API key authenticates exactly one endpoint, the Prometheus metrics at /metrics. It cannot create, edit, pause or delete a monitor, and it is not a general access token for the rest of the app.

</div>
</details>

<details class="mk-faq">
<summary>Is the Python uptime-kuma-api library still maintained?</summary>
<div class="mk-faq__body">

The original Python wrapper has had no commit since September 2023, and its own compatibility table stops at Uptime Kuma 1.23.2. Version 2 is the current line, so the ecosystem has split into several small forks rather than one maintained package.

</div>
</details>

<details class="mk-faq">
<summary>What is the safest way to automate Uptime Kuma today?</summary>
<div class="mk-faq__body">

Push monitors are the one automation surface Uptime Kuma supports on purpose. Your job calls a token URL when it finishes, which covers cron and batch work without driving the private Socket.IO interface at all.

</div>
</details>
