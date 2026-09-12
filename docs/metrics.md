# Metrics

Prometheus exposition on `metrics_bind` (default `127.0.0.1:9090/metrics`).

## Series

Names below are the on-wire names exactly as registered in
`src/observability/metrics.rs` (`observability::metrics::names`) and
sampled in `src/observability/sampler.rs`. Dashboard queries must use
these names verbatim.

| Name | Type | Purpose |
|---|---|---|
| `uptimepage_checks_total{status}` | counter | checks completed, partitioned by terminal status (`up`/`down`/`degraded`/`error`) |
| `uptimepage_checks_errors_total{kind}` | counter | error breakdown by `kind`; currently only `circuit_open` is emitted (a check skipped because its host breaker was open) |
| `uptimepage_check_redirects_total{outcome}` | counter | HTTP redirect hops (`followed` / `limit_exceeded` / `invalid_location` / `blocked_scheme`) |
| `uptimepage_circuit_breaker_state_changes_total{from,to}` | counter | breaker state transitions |
| `uptimepage_storage_writes_total{store,result}` | counter | batcher flush outcomes |
| `uptimepage_storage_dropped_results_total{reason}` | counter | rows dropped before reaching the sink. `queue_full` / `pool_saturated` / `target_in_flight` are check results; `flow_run_write_failed` and `flow_run_buffer_full` are flow-run telemetry, and `heartbeat_ping_write_failed` is the heartbeat ping log — all three are dropped rather than retried forever because the verdict they describe reaches storage by its own path |
| `uptimepage_notifications_total{transport,outcome}` | counter | alert notification sends attempted, by transport and `outcome` (`sent` / `failed` / `deferred`, the last being a send the transport's own budget held back for retry). Attempts, not incidents: a page that succeeds on its third try counts three times |
| `uptimepage_notifications_failures_total{transport}` | counter | notification sends that returned an error, by transport. Counts every failed attempt, including ones a later retry recovers, and a channel whose stored config would not build at all (no send was made, so it contributes no delivery duration) |
| `uptimepage_alerts_held_maintenance_total` | counter | incident alerts held because the monitor is inside a maintenance window that silences paging. Each held alert is recorded as a marker row and pages when the window ends if its incident is still open, so a rise here is deferred paging, not lost paging |
| `uptimepage_alerts_dropped_total{reason}` | counter | incident paging signals dropped before reaching the escalation engine, by `NotificationReason` (`opened`/`escalated`/`resolved`/`reopened`/`no_data`/`data_resumed`). A lifecycle change never blocks on paging throughput, so a saturated signal channel drops here; the incident row stays in Postgres for the reconcile sweep |
| `uptimepage_notifications_dead_lettered_total{transport}` | counter | incident pages that exhausted all retries without delivering, by transport |
| `uptimepage_channels_failing{transport}` | gauge | notification channels whose failure run has reached `escalation.channel_failure_limit`, by transport. Unlike the counters this holds a non-zero value for as long as the endpoint stays dead, so a broken channel bound only to quiet monitors is still visible |
| `uptimepage_notification_delivery_ms{transport}` | histogram | time one notification send took, by transport. Recorded for sends that reached the transport, so a deferred or unbuildable one leaves no sample |
| `uptimepage_alerts_damped_total` | counter | incident alerts held because the monitor keeps failing and recovering. A hold is not a drop: the alert still pages if the incident is open past `escalation.flap_hold_secs`, so sustained growth here means noisy monitors, not lost alerts |
| `uptimepage_telegram_send_deferred_total` | counter | Telegram sends held back by the per-bot/per-chat send budget rather than sent immediately. Sustained growth means the central bot is rate-limit bound |
| `uptimepage_host_throttle_waits_total{kind}` | counter | per-(org,host,port) (`kind=host`) or per-TLD registry (`kind=rdap`) throttle acquire attempts. The `rdap` label covers WHOIS lookups too, since both share the per-TLD bulkhead |
| `uptimepage_host_throttle_drops_total` | counter | host-bulkhead rejections — `kind=host` over-cap checks recorded as `degraded` without firing alerts. RDAP drops do NOT increment this counter; they fall through to the sticky last-good path (see `domain_expiry_stale_served_total`) |
| `uptimepage_rdap_singleflight_total{outcome}` | counter | registry-lookup singleflight outcome per domain (RDAP or WHOIS) — `hit` (cached, no outbound request) or `miss` (fetcher invoked) |
| `uptimepage_domain_expiry_stale_served_total{kind}` | counter | times the domain-expiry executor served a cached last-good answer instead of a fresh probe. `kind` distinguishes the cause: `throttled`, `timeout`, `lookup_error`, or `fresh_error` (no usable last-good — emitted as a real `Error` instead) |
| `uptimepage_flow_runs_total{outcome}` | counter | browser flow runs completed, by `outcome`: `passed`, `failed` (a step failed — the journey is down), `budget` (the whole-run deadline arrived first), `engine` (CDP or the browser process broke), `unconfigured` (the check reached a node with no engine). Only `failed` is a verdict on the target; the rest mean this node stopped paying for an answer |
| `uptimepage_domain_expiry_state_write_failed_total` | counter | failures writing the last-good cache row after a successful probe. Sustained values mean the sticky cache is going cold even though probes succeed — typical cause is Postgres write degradation |
| `uptimepage_scheduler_refresh_failed_total` | counter | registry refresh ticks that returned an error from Postgres. Alert on a sustained rate above your normal noise floor; persistent failures put the scheduler into exponential backoff (capped at 10× the configured refresh interval) and keep workers running with cached `ScheduledTarget` snapshots |
| `uptimepage_rdap_singleflight_slots` | gauge | live entries in the in-process registry-lookup singleflight cache. Bounded under normal load by the set of monitored domains; sudden growth signals a code path feeding non-target domains into the cache |
| `uptimepage_scheduler_consecutive_refresh_failures` | gauge | consecutive registry refresh failures since the last success. Primary alarm signal for a stuck scheduler — page when the value stays above 5 for more than a few minutes. Resets to 0 on the first successful refresh |
| `uptimepage_scheduler_refresh_duration_ms` | histogram | wall-clock duration of one registry refresh tick (Postgres query + decode + DashMap diff). p99 climbing into the hundreds of ms means the current full-scan refresh is starting to strain at scale — the trigger for the deferred incremental-sync work |
| `uptimepage_build_info{version}` | counter | set to 1 once at startup so the endpoint is never empty |
| `uptimepage_check_duration_ms` | histogram | per-check wall time. The `uptimepage_check_*_ms` family is exposed as histogram buckets (not summary quantiles) so percentiles aggregate correctly across regions; query with `histogram_quantile()` |
| `uptimepage_check_dns_ms` | histogram | DNS resolution latency (recorded in the hickory wrapper) |
| `uptimepage_check_connect_ms` | histogram | TCP connect latency (every HTTP check connects fresh) |
| `uptimepage_check_tls_ms` | histogram | TLS handshake latency (per HTTPS check) |
| `uptimepage_check_ttfb_ms` | histogram | time-to-first-byte: request sent to response headers |
| `uptimepage_http_access_diagnostics_total{outcome,kind,provider,confidence}` | counter | failed HTTP checks attributed to the edge in front of the origin. `kind` separates access interference (CDN/WAF block) from an unreachable origin and a downed origin tunnel (edge served its own error page); `matched` uses bounded kind/provider/confidence enums. `unmatched` is narrow by design — a failed 403, or a failure whose headers look like a Cloudflare error page that no signature matched — so a relayed origin 5xx, which is a correct non-attribution, stays out of it. Track the unmatched ratio to catch provider-template drift without putting customer hosts in metric labels |
| `uptimepage_flow_step_duration_ms{op}` | histogram | wall time of one flow step, by `op` (`goto`/`fill`/`click`/`wait_for`/`assert_text`/`assert_url`). Steps the run never reached are excluded, so the distribution only covers work that happened. A `wait_for` p95 climbing toward the monitor's `step_timeout` is the early warning before the journey starts failing |
| `uptimepage_storage_batch_size` | histogram | flush batch sizes |
| `uptimepage_storage_write_duration_ms` | histogram | flush durations |
| `uptimepage_telegram_send_wait_ms` | histogram | wait imposed on a Telegram send by the send budget before its slot opened |
| `uptimepage_targets_total` | gauge | targets in this process's scheduler registry (sampled). Non-zero only where in-process probing runs; a brain doing agent-only probing reports 0 by design — use `uptimepage_targets_enabled` for the configured-monitor count |
| `uptimepage_targets_enabled{kind}` | gauge | configured enabled monitors counted from Postgres, by `kind`. Slow-cadence inventory gauge, scrape-cached so request load never reaches Postgres; correct on a brain regardless of where probing runs |
| `uptimepage_users_active` | gauge | non-deleted user accounts counted from Postgres. Slow-cadence inventory gauge, scrape-cached |
| `uptimepage_notification_channels{kind}` | gauge | enabled notification channels in live orgs, counted from Postgres by `kind`. The values are the ones `uptimepage_notifications_total` carries as `transport`, so "configured" and "actually paging" join on one label — a kind with channels and no sends is a transport people set up and never hear from. A soft-deleted org drops out at once; an unverified email channel is counted even though it cannot deliver. Slow-cadence inventory gauge, scrape-cached, so every panel needs `max by (kind)` to survive a blue/green overlap |
| `uptimepage_notification_channel_orgs{kind}` | gauge | live organisations holding at least one enabled channel of a `kind`. Adoption breadth rather than volume: one org with fifty webhooks moves `notification_channels` and not this. Do not sum across kinds — an org using two transports is counted in both; use `uptimepage_orgs_with_channels` for the total |
| `uptimepage_orgs_with_channels` | gauge | live organisations with any enabled notification channel. Counted once over all kinds, which the per-kind gauge cannot give. The ceiling on how many orgs a page can reach, not the floor: an org whose only channel is an unconfirmed email address is counted and still hears nothing |
| `uptimepage_subscriptions{status}` | gauge | live accounts per subscription `status` (`none` / `active` / `past_due` / `canceled`). `past_due` is how many customers are inside their grace window right now; a step up there is failed cards, not churn yet |
| `uptimepage_billing_webhooks_total{outcome}` | counter | payment-provider webhooks accepted, by `outcome`. `applied` is normal; `duplicate` and `stale` are the provider's own redelivery and reordering and are expected. `unmatched` (a payment or subscription event naming an account we do not know; events that carry no account by nature, such as a customer being created, count as `applied`) and `foreign` (a subscription that is not the account's live one) should not be routine |
| `uptimepage_billing_webhook_rejected_total{reason}` | counter | payment-provider webhooks not acted on, by `reason`. `signature` at a steady trickle means the endpoint secret in config disagrees with the provider's; `failed` answered 5xx so the provider retries, and a sustained rate means every retry hits the same wall, usually a price the `plan_prices` table does not carry |
| `uptimepage_process_start_time_seconds` | gauge | Unix time this process started. `changes(...[1h]) > 0` is the restart signal the billing alert rules read: a counter that reset with the process, then moved before the first scrape, has no `increase` to show |
| `uptimepage_billing_provider_cancel_failed_total` | counter | subscriptions the app decided to end at the payment provider (a grace window that ran out, or a second live subscription that can serve nobody) where the cancel call did not leave it `canceled` in the provider's own view. A refusal for a subscription the provider already reports as `canceled` is not counted; `paused`, an unknown status, or no answer at all is. Nothing retries, so each one may keep charging the customer until ended by hand in the provider dashboard; the app log line `could not end a subscription at the provider` names the account and subscription. Should sit at zero |
| `uptimepage_account_deletions_requested_total` | counter | account deletions scheduled. Reversible until the grace window closes, so this is the churn signal that still has something to act on; the matching audit rows are `org_audit_log` `action = 'user.deletion_requested'` |
| `uptimepage_credential_changes_total{action,origin,provider}` | counter | sign-in methods added to or removed from an account. `origin=signup` is the credential the account was created with and dominates the `linked` series, so alert on `email_match` specifically rather than on all links. `origin=email_match` means a provider let itself in on an address it attested and nobody clicked add; `origin=session` means the account holder asked for it from their settings. A rise in `linked`+`email_match` without matching sign-ups is what a provider attesting addresses it does not own looks like. The matching rows are `credential_events` |
| `uptimepage_credential_link_refused_total{reason}` | counter | link callbacks whose state named one account while the live session was another. `reason=no_session` is routine: the session lapsed while the user sat on the provider's consent screen. `reason=other_user` should sit at zero — the state alone is not allowed to authorise attaching a credential — so an increment there is a leaked state being replayed or a broken guard. `reason=identity_taken` means a completed dance offered a provider account that already opens somebody else's account: a handful is someone confusing two logins, a rate of them is someone hunting. Both are worth alerting on |
| `uptimepage_passkey_login_refused_total{reason}` | counter | passkey sign-ins refused after the browser had already answered the challenge. `assertion_rejected` is a signature that did not verify against the stored credential. `unidentifiable_credential` and `no_passkey_on_account` mean the assertion resolved to nothing this deployment can back it with — a user handle naming no account, or an account holding no passkey for this host. None of the three is routine, so a run of them is a replayed assertion or a credential that outlived the row behind it |
| `uptimepage_passkey_counter_stalled_total` | counter | sign-ins where a hardware authenticator's signature counter did not advance past the stored value, which WebAuthn treats as a possible clone. Synced passkeys report zero and never reach this, so anything here is a hardware key worth looking at rather than background noise |
| `uptimepage_email_admission_total{surface,outcome,risk}` | counter | addresses the disposable-email gate acted on. `outcome=refused` on a signup surface rising sharply is the shape of scripted abuse; a trickle of `flagged` is ordinary. Only acted-on addresses are counted, so this is a rate to alert on, not a funnel — a clean address increments nothing |
| `uptimepage_disposable_corpus_domains` | gauge | domains in the live disposable-email corpus, after the pinned floor is applied. Smaller than the row count in `disposable_email_domains`, which stores the raw upstream union |
| `uptimepage_disposable_corpus_updated_timestamp_seconds` | gauge | unix time of the last refresh that actually replaced the corpus. Staleness is `time() - value`; a gauge holding an age would be wrong between the hours-apart refreshes that set it. Only a successful refresh moves it, so a dead upstream or a list the sanity guards keep rejecting shows up as an age that climbs. Absent until the first refresh lands, so alert with `absent()` too |
| `uptimepage_orgs_emptied_total` | counter | organisations that went from having monitors to having none. Catches the org that stops using the product without ever asking to be deleted, which no deletion counter sees. Tracked on the business board rather than alerted, because a momentary empty is indistinguishable here from a walkout; the matching audit rows are `org_audit_log` `action = 'target.deleted'` / `'target.bulk_deleted'` |
| `uptimepage_workers_in_flight` | gauge | current worker-pool semaphore depth (sampled). Emitted by every probing process, so on a brain doing agent-only probing the real value is on the agent's `role=probe` series, not the brain's near-zero one |
| `uptimepage_result_queue_depth` | gauge | depth of the result channel buffer (sampled). Present on both the agent (egress to the control plane) and the brain (ingest to storage); separate them by `role` |
| `uptimepage_circuit_breakers_open` | gauge | currently-open breakers (sampled). Probe-side — read the `role=probe` series |
| `uptimepage_monitors_unmonitored` | gauge | monitors whose covering probes have all gone silent (no fresh results), from the silence sweep. Distinct from down: these have no data at all |
| `uptimepage_agent_up{region,agent}` | gauge | 1 if a regional agent checked in within the staleness window, else 0. Emitted by the control plane from `agents.last_seen_at`, so it covers remote agents that Alloy can't scrape. Per-agent series can freeze on agent removal, so alerts use `uptimepage_agents_enabled_down` |
| `uptimepage_agent_last_seen_age_seconds{region,agent}` | gauge | seconds since a regional agent last checked in. Climbs unbounded when an agent goes dark |
| `uptimepage_agents_enabled_down` | gauge | count of enabled regional agents currently past the staleness window. Recomputed every sweep so it never latches. The dead-man signal for a probe region going dark |
| `uptimepage_region_agents_total{region}` | gauge | enabled agents configured for a region — the quorum denominator. Brain-side from the `agents` table |
| `uptimepage_region_agents_up{region}` | gauge | enabled agents in a region fresh within the staleness window — the quorum numerator. `up / total` is the region's health fraction; `up == 0` means the region's agents have all gone stale. Recomputed each sweep; like the per-agent gauges it can freeze if a region's last agent is removed. Covers agents Alloy can't scrape |
| `uptimepage_region_checks_window{region}` | gauge | checks completed in a region over the recent sampling window. Brain-side count from ClickHouse, so it covers remote agents Alloy can't scrape. Only regions with results in the window appear |
| `uptimepage_region_checks_up_window{region}` | gauge | checks that returned up in a region over the recent window. Divide by `uptimepage_region_checks_window` for the success ratio |
| `uptimepage_region_check_latency_p95_ms{region}` | gauge | approximate p95 check latency in a region over the recent window, in ms. Goes stale for a dark region (no new rows), so gate panels on `uptimepage_region_agents_up` |
| `uptimepage_check_error_class_checks{class,family}` | gauge | failed checks over the recent sampling window, by error class and by the family it belongs to (`internal`, `transport`, `verdict`, `other`). Brain-side count from ClickHouse across every org. Every known class is reported each sweep, zero included, so a series never freezes at a stale value |
| `uptimepage_check_error_class_top_monitor_share{class,family}` | gauge | fraction of a class's checks contributed by its single largest monitor, 0..1. A lower bound where a class spans several raw error strings. Near 1 on a `family=internal` class means one monitor owns a probe-side failure; the monitor's id goes to the log, never to a label |
| `uptimepage_check_error_class_sweep_age_seconds` | gauge | seconds since the error-class sweep last completed. Every class gauge holds its last value when a sweep fails, so an alert built on them must gate on this to tell a quiet fleet from a sweep that stopped running |
| `uptimepage_check_error_class_truncated` | gauge | 1 when the sweep hit its row cap, else 0. Raw error strings interpolate hostnames and IPs, so their count grows with the fleet; past the cap a low-volume class publishes as 0 and its alert becomes unfireable |
| `uptimepage_pg_pool_size` | gauge | total connections held in the sqlx Postgres pool (idle + in-use). Bounded above by `storage.postgres.max_connections` |
| `uptimepage_pg_pool_idle` | gauge | connections sitting idle in the Postgres pool. A persistent `idle = 0` alongside `in_use` at the max is the saturation signal |
| `uptimepage_pg_pool_in_use` | gauge | connections checked out of the Postgres pool right now (`size − idle`). Alert on a sustained high `in_use / size` ratio |
| `uptimepage_process_resident_bytes` | gauge | resident set size of the process (`VmRSS`) in bytes. Linux only — absent on non-Linux dev runs. Early-warning signal for slow leaks ahead of the OOM killer |
| `uptimepage_clickhouse_max_part_count_for_partition` | gauge | ClickHouse `MaxPartCountForPartition` (sampled from `system.asynchronous_metrics`). Partition-explosion early warning — climbs toward `parts_to_throw_insert` (default 3000) if a high-cardinality column is added to `PARTITION BY` |
| `uptimepage_http_requests_total{method,route,status}` | counter | inbound HTTP requests handled. `route` is `MatchedPath` (the path-pattern with placeholders) — cardinality bounded by the static router table, never by per-tenant ids. `status` is bucketed `2xx`/`3xx`/`4xx`/`5xx`/`other`; query `sum by (status) (rate(...[5m]))` for the SLO ratio |
| `uptimepage_http_request_duration_ms{method,route}` | histogram | inbound HTTP request latency, exposed as summary quantiles (single web instance, no cross-instance merge). Query `name{quantile="0.99"}` for tail latency per route |
| `uptimepage_http_responses_inflight` | gauge | inbound HTTP requests currently being served. Climbing alongside flat throughput points at handler back-pressure on a downstream pool |
| `uptimepage_ai_crawler_requests_total{bot,kind,section}` | counter | assistant crawler fetches of the marketing surface, the half the browser tracker cannot see because crawlers run no JavaScript. `kind` is the label that carries meaning: `user-fetch` means an agent was dispatched because someone asked a question seconds earlier, so it tracks live citation, while `crawler` is corpus building for answers weeks away. Do not average the two. `bot` comes from a fixed table, so an unrecognised `User-Agent` opens no series; `section` is coarse (`home`, `blog`, `docs`, `compare`, `vs`, `tools`, `index`, `landing`) so the sitemap cannot become the metric |
| `uptimepage_ratelimit_drops_total{scope}` | counter | HTTP 429s from the per-account / per-user rate-limit middleware. `scope` is the same string carried in the error body (`per_account_api_writes`, `per_user_bulk_ops`, …) so dashboards can join with `record_quota_event` audit rows. Abuse signal — a tenant hammering the API spikes one scope before shared resources notice |

Scrape interval of 15 s is plenty — counters are written from hot tokio tasks; histograms aggregate per bucket without lock contention.

**Histogram exposition.** Two forms. The `uptimepage_check_*_ms` family is
configured with explicit buckets and exported as a Prometheus **histogram**
(`name_bucket{le="..."}` plus `name_sum` / `name_count`); query it with
`histogram_quantile(0.99, sum(rate(name_bucket[5m])) by (le))` so percentiles
pool correctly across regional agents. Every other `*_ms` / `*_size` histogram
keeps the default exposition, a Prometheus **summary** with precomputed
quantile series (`name{quantile="0.5|0.9|0.95|0.99|0.999"}`) plus `name_sum`
and `name_count`; query those as `name{quantile="0.99"}` directly. Gauges
carry no `org_id` label, these are single-instance operator metrics, not
per-tenant.

**Scrape labels.** The collector stamps two labels the app does not set: `role` (`control-plane` on the brain, `probe` on a regional agent) and, on probe series, `region`. The brain and a probe both emit the prober and pipeline metrics (`check_*`, `workers_in_flight`, `circuit_breakers_open`, `result_queue_depth`, `storage_*`, `process_resident_bytes`), so filter by `role` to read the one you mean rather than summing two processes. The Ops dashboard pins probe panels to `role=probe` and filters them by a `$region` variable; the Business dashboard reads the control-plane-only inventory gauges.

The `uptimepage_region_*` gauges are different: the brain emits them with a `region` label it sets itself (from the `agents` table and from ClickHouse), not a collector-stamped scrape label. They are the per-region surface on a SaaS control plane, where the regional agents are not scraped at all: liveness and quorum from the `agents` table (`region_agents_up` / `_total`), throughput and latency from ClickHouse (`region_checks_window` / `_up_window` / `region_check_latency_p95_ms`). One scrape point, cost scales with regions, not tenants or fleet size.

**Error classes.** A probe's raw error string interpolates hostnames, IPs and vendor text, so it is unbounded and carries customer data — it can never label a metric. `uptimepage_check_error_class_*` label a bounded class instead, grouped into four families. `verdict` is the target judged unhealthy, `transport` is an exchange that never completed, `other` is error text nobody has classified yet, and its size is the measure of how much surface is still free text. `internal` is the narrow one: this node could not run the check at all, so nothing in it depends on what the target did and none of it has a legitimate steady state. That is the family worth alerting on — the other three sit high and stay high on a healthy fleet, because customer sites really are down. A failure caused by the target belongs elsewhere even when our code raised it: a body read that dies mid-stream is the connection, and a page that inflates past the decoded cap does so on every check forever.

## OpenTelemetry tracing

Spans are exported over OTLP/HTTP (protobuf) when **both**
`observability.tracing_enabled` and `observability.grafana.enabled` are
`true`. The exporter targets `observability.grafana.otlp_endpoint`
(the OTLP base; `/v1/traces` is appended) and authenticates with
`Authorization: Basic base64(instance_id:api_key)`. The destination is
any OTLP/HTTP collector — Grafana Cloud Tempo, Jaeger, an OpenTelemetry
Collector, etc.

- `api_key` is read only from
  `UPTIMEPAGE_OBSERVABILITY__GRAFANA__API_KEY` — never from a file.
- Sampling is parent-based over a head ratio
  (`grafana.trace_sample_ratio`, default `0.05`); a sampled parent keeps
  its children.
- Resource attributes: `service.name = uptimepage`,
  `service.version` = the build version.
- Disabled by default and **zero-cost when off**: no exporter is built,
  no network egress, no per-check overhead.
- A batch exporter ships spans in the background; it is flushed and
  stopped on graceful shutdown so the final spans are not lost. A
  transport build failure logs a warning and the service continues
  without traces — telemetry never takes down monitoring.

Inconsistent settings (export on but endpoint/instance/key missing, or
a sample ratio outside `[0.0, 1.0]`) fail fast at startup as a config
error, not a runtime surprise. See
[Configuration](configuration.md) for the keys and env overrides.

## HTTP connection phase timings

Every HTTP check opens a fresh connection (no pool — a monitor probes each target once per interval, so a pool rarely reused a socket, and fresh-connect is what lets the probe attribute time to each phase). `check_dns_ms`, `check_connect_ms`, and `check_tls_ms` are timed during that establishment and `check_ttfb_ms` from request-send to response headers. The same four values are written per-check into ClickHouse, which is what powers the detail-page latency-breakdown chart.
