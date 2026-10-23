+++
# Written by Artem Senenko. Attribution required under AGPL-3.0 sections 7(b) and 7(c); see "Blog posts" in LICENSING.md.
title = "Building an uptime monitor in Rust: one binary, two databases"
date = "2026-06-30"
updated = "2026-10-17"
slug = "building-an-uptime-monitor-in-rust"
excerpt = "The Rust build behind Uptimepage: a custom hyper client doing ~130K checks a second on one core, a single-heap scheduler, and ClickHouse rollups."
tags = ["rust", "clickhouse", "monitoring", "devops"]
draft = false
+++

I spent the last few months building Uptimepage, an [open-source uptime monitor](/open-source-uptime-monitoring) and status page written in Rust. This is the build story: the decisions that shaped it, the parts I rewrote, and the numbers that came out the other side.

The whole thing ships as one self-contained binary of about 23 MB, plus Postgres and ClickHouse. You can `docker compose up` and self-host it, or use the hosted tier. Source is AGPL-3.0 on [GitHub](https://github.com/uptimepage/uptimepage).

## Why one binary

A status page is a small surface with a lot of moving parts behind it: a scheduler, probe workers, an HTTP client, a time-series writer, an incident detector, an alerting fan-out, a web UI, a JSON API. The usual answer is a handful of services and a message bus between them.

I went the other way. One process, one binary, the same image whether it runs the control plane or a remote probe. No queue to operate, no version skew between services, no "which container is wedged" at 3am. The cost is that you have to be careful about what shares a thread and what can stall what. Most of the engineering below is about keeping those boundaries clean inside a single process.

Stack: Rust 1.95 (edition 2024), Tokio, Axum, Askama for compile-time HTML templates, HTMX for partial swaps so there is no SPA framework to ship. The API stays the single source of truth because every UI mutation hits the same `/api/v1/*` endpoint a script would.

## Two databases, on purpose

Monitors are low-cardinality relational data that gets mutated by API calls: targets, regions, channels, incidents, plans. That is Postgres. Check results are append-only, high-cardinality, and almost always queried by time range. That is ClickHouse.

Trying to force one of those into the other is where uptime monitors usually fall over. Putting billions of check results in Postgres turns every dashboard query into a sequential scan. Putting your relational config in ClickHouse means fighting its update model forever. So: Postgres for the world, ClickHouse for the firehose. Both run their migrations at process startup, so there is no separate migrator to forget. I go deeper on that split, the column codecs, and per-row retention in [a post on why I run both](/blog/postgres-vs-clickhouse-uptime-monitor).

## The HTTP client I did not want to write

The first version used a popular high-level HTTP client. It worked, but a monitor is a weird HTTP workload: you connect once per target per interval, you never reuse the connection, and you care about the timing of each phase more than the body.

So I dropped down to `hyper` and `hyper-util` with `rustls` and built a connector that times DNS resolution, TCP connect, and the TLS handshake as separate numbers, then runs the request over `hyper::client::conn` and aborts the connection task the moment the body is read. Each result carries `dns_ms`, `connect_ms`, `tls_ms`, and `ttfb_ms` as distinct columns, which is what makes "it got slow but it is the DNS, not your server" a thing the dashboard can actually say.

The rewrite paid for itself. On a single core the client sustains around 130K checks a second at saturation, roughly 7.7 microseconds per check. That was a 44 to 56 percent throughput gain over the old path. A chunk of it was a `url::parse` call hiding in the redirect policy that cost 7.5 percent on its own and just vanished. Two cores get to about 153K. Scaling goes sub-linear past four cores because of shared HTTP/2 connection state and the pool mutex, which is a fine problem to have for this workload.

These numbers come from a laptop and a load-test binary, so I treat them as regression detection, not capacity planning. The design goal is around 50K concurrent in-flight checks per node with under 50ms p99 of per-check overhead on top of the network.

## One heap, not a timer per target

The naive scheduler spawns a timer task per monitor. The tasks themselves are cheap. What falls apart at fleet size is everything around them: a thousand independent clocks, a handle map with a lock on it, and one copy of the config per task to keep in step.

Instead there is a single driver task that owns one `BinaryHeap<Reverse<Due>>`, a min-heap keyed by the next due `Instant` for the whole fleet. Nothing in it needs a lock, because nothing in it leaves the task. Each target gets a deterministic jitter offset hashed from its UUID so a thousand monitors on a 60-second interval do not all fire on the same tick. Generation and sequence counters mean a re-scheduled target supersedes its stale heap entry instead of double-firing. The registry refresh that pulls config from Postgres runs on its own task with exponential backoff, so a Postgres hiccup never stalls dispatch: the scheduler keeps running on what it last knew.

The whole scheduler, with the code and the tests that back each of those claims, is [its own post](/blog/rust-scheduler-one-heap).

## Failure isolation inside one process

Because everything is one process, one bad target cannot be allowed to take down the rest. Three patterns do most of that work. Per-monitor circuit breakers trip when a monitor keeps timing out, so its next checks skip the probe instead of tying up a worker on a timeout, repeat the last timeout so the cause stays visible, and probe again after a cooldown. A per-tenant host throttle bulkhead caps how many checks can be in flight against one host at once; over the cap, a check is recorded as throttled and degraded rather than piling on, and it never pages. And singleflight on RDAP collapses domain-expiry checks for the same domain across many tenants into one upstream probe, with sticky last-good state so a flaky registrar does not flip the monitor red.

The worker pool itself is a task-per-dispatch gated by a semaphore sized to a max-concurrency setting, with an SSRF guard filtering resolved IPs before any connect.

## Modeling time series in ClickHouse

The `check_results` table is a `MergeTree` ordered by `(org_id, target_id, region, timestamp)`. The `org_id` leads the sort key so each tenant gets a sparse-index slice, but it is deliberately kept out of the partition key (partition is by day) so we do not end up with millions of partitions.

The columns are where the storage savings live. Timestamps are `DateTime('UTC')` with `CODEC(DoubleDelta, ZSTD(1))`: check intervals are near-constant, so DoubleDelta crushes the gaps to almost nothing. The numeric phase columns (`duration_ms`, `dns_ms`, `connect_ms`, `tls_ms`, `ttfb_ms`, response code and size) use `CODEC(T64, ZSTD(1))`. `region` and `agent_id` are `LowCardinality(String)`, and status is an `Enum8`.

Retention is per row. There is a `ttl_days` column with `TTL timestamp + toIntervalDay(ttl_days)`, stamped from the org's plan at write time. A plan that buys longer history needs zero schema change: new rows just carry a bigger number.

On top of the raw table sit two `AggregatingMergeTree` materialized views: a per-minute rollup kept 30 days and an hourly rollup kept 13 months, both holding `quantilesState` for p50/p95/p99 and per-status counts. Reads route by range. Anything inside 30 days hits the minute rollup, older ranges hit the hour rollup, and raw reads are capped at a 90-day span. A dashboard asking for "last 24h p99 latency" never touches a raw row.

## Regional probes without a second brain

You can run probes in multiple regions, but I did not want each region to be its own little system. So an agent is the same binary in agent mode, running as a stateless probe with no database, no web, no alerting. Adding a region adds execution capacity, never a second control plane.

An agent pulls its region's config from the control plane with ETag handling, serves its last-known config if the control plane blips, and pauses if its token is revoked. It ships results back in batches that reuse one UUID across retries so a lost ack cannot double-count. Region and agent identity are derived server-side from the bearer token, never sent in the payload, so a probe cannot claim to be somewhere it is not. A separate long-poll loop handles interactive "check now" so a button press in the UI runs on a real remote probe within milliseconds.

Region is the partition dimension end to end, so every read can slice by region: per-region latency series, per-region incident scope, "down in Singapore, up in Helsinki."

## Incidents as a follower, not a gatekeeper

The incident detector is a background task that follows the `check_results` stream and writes into the Postgres `incidents` table. The rule I held to: it never touches the hot write path, never gates check execution, and never produces alerts directly.

The detection itself is boring on purpose. Two or more consecutive unhealthy results with no open incident opens one; two or more consecutive healthy results closes it. A small flap threshold absorbs single-result blips. The cross-tenant walk is keyset-paginated so memory stays bounded as tenants grow, and a unique index on open incidents resolves the race when two ticks try to open the same one: only the winner pages.

Opening or resolving fires a non-blocking signal to the escalation engine, which does repeat-until-acknowledged paging on a per-monitor cadence across about fourteen transports (Slack, signed webhooks, Telegram, PagerDuty, ntfy, Pushover, Discord, email, and more), with sharded per-incident locks so a reconcile and an inbound signal can never double-page the same episode. Channel secrets are sealed at rest with AES-GCM and never echoed back.

## Automation as a first-class surface

Because the API is the single source of truth, the rest came almost for free: a self-describing OpenAPI spec with Swagger UI, an [official Terraform provider](/terraform-uptime-monitoring) so you can manage [monitors as code](/blog/monitoring-as-code), and an [MCP server](/blog/mcp-server) so an LLM client can query your monitors and incidents over OAuth, scope-gated and audited, with per-action confirmation on the few write tools.

## Where it is

Uptimepage is live, free to start with no card, and AGPL-3.0 open source. The core is not paywalled: checks, status pages, subscribers, the API, and every alert channel are in the free tier. It monitors itself and serves its own status badges from the running binary, which is the most honest dogfood I could think of. The same instinct runs through the rest of these notes: a monitor should be [the most boring, most trustworthy thing you own](/blog/boring-uptime), and that goes for how it is built too.

The source is on [GitHub](https://github.com/uptimepage/uptimepage) if you want to read the internals or run your own.
