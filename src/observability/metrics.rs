use std::net::SocketAddr;

use anyhow::Context;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

use crate::error::Result;

pub struct MetricsHandle;

// Buckets (ms) for the per-check latency family. Resolution is concentrated
// where probe latencies sit; the top bucket is above the check timeout (10s)
// so a timed-out check stays distinguishable instead of collapsing into +Inf
// and saturating the high percentiles.
const CHECK_LATENCY_BUCKETS_MS: &[f64] = &[
    5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 7500.0, 10000.0, 30000.0,
];

pub fn init(bind: &str) -> Result<MetricsHandle> {
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("parsing metrics_bind '{bind}'"))?;

    PrometheusBuilder::new()
        .with_http_listener(addr)
        // Per-check latencies are emitted by every regional agent and merged
        // across regions, where a quantile of quantiles is wrong. Expose them
        // as histogram buckets so histogram_quantile() aggregates correctly.
        // The other histograms stay summaries: single control-plane instance,
        // no cross-instance merge to get wrong.
        .set_buckets_for_metric(
            Matcher::Prefix("uptimepage_check_".to_owned()),
            CHECK_LATENCY_BUCKETS_MS,
        )
        .context("set check-latency histogram buckets")?
        .install()
        .context("install prometheus exporter")?;

    register_descriptions();
    prime_event_counters();
    metrics::counter!("uptimepage_build_info", "version" => env!("CARGO_PKG_VERSION")).absolute(1);
    metrics::gauge!(names::PROCESS_START_TIME).set(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64()),
    );
    tracing::info!(
        // SAFE: operator metrics bind address, not a peer/user IP
        addr = %addr,
        "metrics listening"
    );
    Ok(MetricsHandle)
}

fn prime_event_counters() {
    // increase() cannot see the first increment of a series that appears
    // mid-range, and these fire rarely enough that it usually does.
    metrics::counter!(names::ACCOUNT_DELETIONS_REQUESTED).increment(0);
    metrics::counter!(names::ORGS_EMPTIED).increment(0);
    for outcome in ["applied", "duplicate", "stale", "unmatched", "foreign"] {
        metrics::counter!(names::BILLING_WEBHOOKS, "outcome" => outcome).increment(0);
    }
    for reason in ["signature", "malformed", "failed"] {
        metrics::counter!(names::BILLING_WEBHOOK_REJECTED, "reason" => reason).increment(0);
    }
    metrics::counter!(names::BILLING_PROVIDER_CANCEL_FAILED).increment(0);
}

fn register_descriptions() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!(
        "uptimepage_checks_total",
        "Total checks completed, labelled by status"
    );
    describe_counter!(
        "uptimepage_alerts_damped_total",
        "Incident alerts held because the monitor is flapping"
    );
    describe_counter!(
        "uptimepage_alerts_held_maintenance_total",
        "Incident alerts held because the monitor is in a maintenance window that silences paging. Each held alert pages when the window ends if its incident is still open"
    );
    describe_counter!(
        "uptimepage_checks_errors_total",
        "Total check errors, labelled by kind"
    );
    describe_counter!(
        "uptimepage_check_redirects_total",
        "HTTP redirect hops, labelled by outcome \
         (followed | limit_exceeded | invalid_location | blocked_scheme)"
    );
    describe_counter!(
        "uptimepage_http_access_diagnostics_total",
        "Failed HTTP checks attributed to the edge in front of the origin. Matched rows are labelled by bounded kind/provider/confidence enums; unmatched covers only a failed 403 or a failure carrying Cloudflare error-page headers that no signature matched, so drift is observable without target labels"
    );
    describe_counter!(
        "uptimepage_circuit_breaker_state_changes_total",
        "Circuit breaker state transitions"
    );
    describe_counter!(
        "uptimepage_storage_writes_total",
        "Storage writes, labelled by store and result"
    );
    describe_counter!(
        "uptimepage_storage_dropped_results_total",
        "Results dropped before storage, labelled by reason"
    );
    describe_counter!(
        "uptimepage_notifications_dead_lettered_total",
        "Incident pages that exhausted all retries without delivering, labelled by transport"
    );

    describe_histogram!(
        "uptimepage_check_duration_ms",
        "Total check duration in milliseconds"
    );
    describe_histogram!(
        "uptimepage_check_dns_ms",
        "DNS resolution latency in milliseconds"
    );
    describe_histogram!(
        "uptimepage_check_connect_ms",
        "TCP connect latency in milliseconds (recorded only when a new connection is established)"
    );
    describe_histogram!(
        "uptimepage_check_tls_ms",
        "TLS handshake latency in milliseconds (recorded only when a new HTTPS connection is established)"
    );
    describe_histogram!(
        "uptimepage_check_ttfb_ms",
        "HTTP time-to-first-byte in milliseconds"
    );
    describe_histogram!(
        "uptimepage_storage_batch_size",
        "Result batch size at flush time"
    );
    describe_histogram!(
        "uptimepage_storage_write_duration_ms",
        "Storage write duration in milliseconds"
    );

    describe_gauge!(
        "uptimepage_targets_total",
        "Targets in this process's scheduler registry. Non-zero only where in-process probing runs; a brain with agent-only probing reports 0 by design — use uptimepage_targets_enabled for the configured-monitor count"
    );
    describe_gauge!(
        "uptimepage_targets_enabled",
        "Configured enabled monitors counted from Postgres, labelled by kind. Inventory gauge refreshed on a slow cadence and scrape-cached, so request load never reaches Postgres. Source of truth for the dashboard monitor count regardless of where probing runs"
    );
    describe_gauge!(
        "uptimepage_users_active",
        "Non-deleted user accounts counted from Postgres. Slow-cadence inventory gauge, scrape-cached"
    );
    describe_gauge!(
        "uptimepage_notification_channels",
        "Enabled notification channels counted from Postgres, labelled by kind. The kind values are the ones uptimepage_notifications_total carries as transport. Slow-cadence inventory gauge, scrape-cached"
    );
    describe_gauge!(
        "uptimepage_notification_channel_orgs",
        "Organisations with at least one enabled channel of a kind, labelled by kind. Not summable across kinds: an org using two transports is counted in both"
    );
    describe_gauge!(
        "uptimepage_orgs_with_channels",
        "Organisations with at least one enabled notification channel, counted once over all kinds"
    );
    describe_gauge!("uptimepage_workers_in_flight", "Checks currently executing");
    describe_gauge!(
        "uptimepage_result_queue_depth",
        "Current depth of the result channel buffer"
    );
    describe_gauge!(
        "uptimepage_circuit_breakers_open",
        "Number of circuit breakers currently in the Open state"
    );
    describe_gauge!(
        "uptimepage_agent_last_seen_age_seconds",
        "Seconds since a regional agent last checked in, labelled by region and agent"
    );
    describe_gauge!(
        "uptimepage_agent_up",
        "1 if a regional agent checked in within the staleness window, else 0, labelled by region and agent. Per-agent series can freeze on agent removal; alert on uptimepage_agents_enabled_down instead"
    );
    describe_gauge!(
        "uptimepage_agents_enabled_down",
        "Count of enabled regional agents currently past the staleness window. Recomputed every sweep so it never latches, the dead-man signal for a probe region going dark"
    );
    describe_gauge!(
        "uptimepage_region_agents_total",
        "Enabled regional agents configured for a region, labelled by region. Denominator of the per-region quorum (how many agents should be checking in)"
    );
    describe_gauge!(
        "uptimepage_region_agents_up",
        "Enabled regional agents in a region that checked in within the staleness window, labelled by region. Numerator of the per-region quorum; 0 means the region's agents have all gone stale. Recomputed each sweep; like the per-agent gauges it can freeze if a region's last agent is removed"
    );
    describe_gauge!(
        "uptimepage_region_checks_window",
        "Checks completed in a region over the recent sampling window, labelled by region. Brain-side count from ClickHouse, so it covers remote agents Alloy can't scrape. Only regions with results in the window appear"
    );
    describe_gauge!(
        "uptimepage_region_checks_up_window",
        "Checks that returned up in a region over the recent sampling window, labelled by region. Divide by uptimepage_region_checks_window for the success ratio"
    );
    describe_gauge!(
        "uptimepage_check_error_class_checks",
        "Failed checks over the recent sampling window, labelled by error class and by the family the class belongs to (internal/transport/verdict/other). Brain-side count from ClickHouse across every org. Every known class is reported each sweep, zero included, so a series never freezes at a stale value"
    );
    describe_gauge!(
        "uptimepage_check_error_class_top_monitor_share",
        "Fraction of an error class's checks contributed by its single largest monitor over the recent sampling window, 0..1, labelled by class and family. A lower bound where one class spans several raw error strings. Near 1 on a family=internal class means one monitor owns a probe-side failure, which is the stuck-monitor signal; the monitor's id goes to the log, never to a label"
    );
    describe_gauge!(
        "uptimepage_check_error_class_sweep_age_seconds",
        "Seconds since the error-class sweep last completed. Every class gauge holds its last value when a sweep fails, so an alert built on them must gate on this to tell a quiet fleet from a sweep that stopped running"
    );
    describe_gauge!(
        "uptimepage_check_error_class_truncated",
        "1 when the error-class sweep hit its row cap, else 0. Raw error strings interpolate hostnames and IPs, so their count grows with the fleet; past the cap a low-volume class publishes as 0 and its alert becomes unfireable"
    );
    describe_gauge!(
        "uptimepage_region_check_latency_p95_ms",
        "Approximate p95 check latency in a region over the recent sampling window, in milliseconds, labelled by region. Goes stale for a dark region (no new rows), so gate dashboard panels on uptimepage_region_agents_up"
    );
    describe_counter!(
        "uptimepage_notifications_total",
        "Alert notification sends attempted, labelled by transport and outcome. A retry counts again, so this is attempts, not incidents"
    );
    describe_counter!(
        "uptimepage_notifications_failures_total",
        "Alert notification sends that returned an error, labelled by transport"
    );
    describe_histogram!(
        "uptimepage_notification_delivery_ms",
        "Time one alert notification send took in milliseconds, labelled by transport"
    );
    describe_gauge!(
        "uptimepage_channels_failing",
        "Notification channels whose failure run has reached the alerting threshold, labelled by transport. Holds while the endpoint stays dead, so it is visible without an incident having to page"
    );
    describe_counter!(
        "uptimepage_alerts_dropped_total",
        "Incident paging signals dropped before reaching the escalation engine, labelled by reason. The incident row stays in Postgres for the reconcile sweep to retry"
    );
    describe_gauge!(
        "uptimepage_monitors_unmonitored",
        "Monitors whose covering probes have all gone silent (no fresh results), sampled by the silence sweep. Distinct from down: these have no data at all"
    );
    describe_counter!(
        "uptimepage_telegram_send_deferred_total",
        "Telegram sends deferred by the per-bot/per-chat send budget rather than sent immediately. Sustained growth means the central bot is rate-limit bound"
    );
    describe_histogram!(
        "uptimepage_telegram_send_wait_ms",
        "Wait imposed on a Telegram send by the send budget before the slot opened, in milliseconds"
    );
    describe_counter!(
        "uptimepage_rdap_singleflight_total",
        "RDAP singleflight outcomes per domain: hit (cached) or miss (fetched)"
    );
    describe_counter!(
        "uptimepage_domain_expiry_stale_served_total",
        "Times the domain_expiry executor served a cached last-good answer instead of a fresh probe, labelled by failure kind"
    );
    describe_counter!(
        "uptimepage_domain_expiry_state_write_failed_total",
        "Failures writing the last-good cache row after a successful probe — sustained values mean the sticky cache is going cold even though probes succeed"
    );
    describe_gauge!(
        "uptimepage_rdap_singleflight_slots",
        "Live entries in the RDAP singleflight cache. Bounded under normal load by the set of monitored domains"
    );
    describe_counter!(
        "uptimepage_scheduler_refresh_failed_total",
        "Registry refresh ticks that returned an error from Postgres — alert on a sustained rate above your normal noise floor"
    );
    describe_gauge!(
        "uptimepage_scheduler_consecutive_refresh_failures",
        "Consecutive registry refresh failures since the last success. Resets to 0 on recovery; primary alarm signal for a stuck scheduler"
    );
    describe_histogram!(
        "uptimepage_scheduler_refresh_duration_ms",
        "Wall-clock duration of one registry refresh tick (Postgres query + decode + DashMap diff). p99 climbing past a few hundred ms means the full-scan refresh is starting to strain at scale — switch to incremental sync"
    );
    describe_gauge!(
        "uptimepage_pg_pool_size",
        "Total connections currently held in the sqlx Postgres pool (idle + in-use). Bounded above by the configured max_connections"
    );
    describe_gauge!(
        "uptimepage_pg_pool_idle",
        "Connections sitting idle in the Postgres pool. A persistent idle = 0 alongside in_use at the max means saturation"
    );
    describe_gauge!(
        "uptimepage_pg_pool_in_use",
        "Connections checked out of the Postgres pool right now (size − idle). Alert when the ratio against pool_size stays high for several minutes"
    );
    describe_gauge!(
        "uptimepage_process_resident_bytes",
        "Resident set size of the uptimepage process in bytes (VmRSS on Linux). Early-warning signal for slow leaks and unbounded growth before the OOM killer triggers; not exposed on non-Linux platforms"
    );
    describe_gauge!(
        "uptimepage_clickhouse_max_part_count_for_partition",
        "ClickHouse MaxPartCountForPartition (sampled from system.asynchronous_metrics). Early warning for partition explosion — climbs toward parts_to_throw_insert (default 3000) if a high-cardinality column is ever added to PARTITION BY"
    );
    describe_counter!(
        "uptimepage_http_requests_total",
        "HTTP requests handled, labelled by method, MatchedPath route, and status class (2xx/3xx/4xx/5xx/other). The route label is the path-pattern with placeholders so cardinality is bounded by the router's static route table"
    );
    describe_histogram!(
        "uptimepage_http_request_duration_ms",
        "HTTP request latency in milliseconds, labelled by method and MatchedPath route. Primary SLO signal — query the {quantile=\"0.99\"} series for tail latency"
    );
    describe_gauge!(
        "uptimepage_http_responses_inflight",
        "HTTP requests currently being served. Climbing alongside flat throughput means handlers are blocking on something (usually a downstream pool); a release-valve signal for upstream contention"
    );
    describe_counter!(
        "uptimepage_flow_runs_total",
        "Browser flow runs completed, labelled by outcome: passed, failed (a step failed — the journey is down), budget (the whole-run deadline arrived first), engine (CDP or the browser process broke), unconfigured (the check reached a node with no engine). Only `failed` is a verdict on the target"
    );
    describe_histogram!(
        "uptimepage_flow_step_duration_ms",
        "Wall-clock duration of one flow step in milliseconds, labelled by op (goto/fill/click/wait_for/assert_text/assert_url). Steps the run never reached are excluded. A wait_for p95 climbing toward step_timeout is the early warning before the journey starts failing"
    );
    describe_counter!(
        "uptimepage_ratelimit_drops_total",
        "Per-account and per-user rate-limit rejections (HTTP 429), labelled by `scope` (the same string carried in the error response — e.g. `per_account_api_writes`, `per_user_bulk_ops`). Abuse signal: a sudden rate growth on one scope is the first indicator of a single tenant hammering the API"
    );
    describe_counter!(
        "uptimepage_account_deletions_requested_total",
        "Self-service account deletions requested. Churn signal, not a health signal: every increment is a customer leaving inside a grace window that is still reversible, and nothing else in the stack observes it"
    );
    describe_counter!(
        "uptimepage_credential_changes_total",
        "Sign-in methods added to or removed from an account, labelled by `action` (linked | unlinked), `origin` (signup | email_match | session) and `provider`. `signup` is the credential the account was created with and dominates the `linked` series, so alert on `origin=\"email_match\"` specifically rather than on all links. `email_match` means a provider let itself in on an address it attested and nobody clicked add — a rise there without matching sign-ups is what a provider attesting addresses it does not own looks like"
    );
    describe_counter!(
        "uptimepage_credential_link_refused_total",
        "Link callbacks whose state named one account while the live session was another, labelled by `reason`. `no_session` is routine — the session lapsed while the user was on the provider's consent screen. `other_user` should sit at zero: the state alone is not allowed to authorise attaching a credential, so an increment there is a leaked state being replayed or a bug in the guard. `identity_taken` means a completed dance offered a provider account that already opens somebody else's account — a handful is someone confusing two logins, a rate of them is someone hunting"
    );
    describe_counter!(
        "uptimepage_ai_crawler_requests_total",
        "Assistant crawler fetches of the marketing surface, labelled by `bot`, `section`, and `kind`. `kind=user-fetch` is an agent dispatched because a person asked a question seconds earlier, so it tracks live citation; `kind=crawler` is corpus building for answers weeks away. The browser tracker sees assistant referrals but never these fetches, because crawlers run no JavaScript"
    );
    describe_counter!(
        "uptimepage_orgs_emptied_total",
        "Deletes that took an organisation from having monitors to having none. The shape a customer walking out leaves behind when they clear the account by hand instead of deleting it"
    );
    describe_gauge!(
        "uptimepage_disposable_corpus_domains",
        "Domains in the live disposable-email corpus"
    );
    describe_gauge!(
        "uptimepage_disposable_corpus_updated_timestamp_seconds",
        "Unix time of the last refresh that actually replaced the disposable-email corpus. A timestamp rather than an age so `time() - value` stays correct between refreshes, which are hours apart. Only successful refreshes move it, so a stalled upstream or a list the sanity guards keep rejecting shows up here as an age that keeps climbing. Absent until the first refresh lands"
    );
    describe_counter!(
        "uptimepage_billing_webhooks_total",
        "Payment-provider webhooks accepted, labelled by `outcome` (applied | duplicate | stale | unmatched | foreign). `applied` is the normal case; `duplicate` and `stale` are the provider's own redelivery and reordering and are routine. `unmatched` names an account we do not know and `foreign` a subscription that is not the account's live one, and neither should be routine"
    );
    describe_counter!(
        "uptimepage_billing_webhook_rejected_total",
        "Payment-provider webhooks not acted on, labelled by `reason` (signature | malformed | failed). `signature` at a steady trickle means the endpoint secret in config does not match the provider's; `failed` answered 5xx so the provider retries, and a sustained rate means every retry is failing the same way, usually a price the `plan_prices` table does not know"
    );
    describe_gauge!(
        "uptimepage_process_start_time_seconds",
        "Unix time this process started. `changes()` over a window is the restart signal: a counter that reset with the process has no `increase` to show for an event landing before the first scrape"
    );
    describe_counter!(
        "uptimepage_billing_provider_cancel_failed_total",
        "Subscriptions we decided to end at the payment provider where the cancel call failed and the provider did not show the subscription ended. Each one may keep charging the customer until ended by hand in the provider dashboard, so this should sit at zero; the app log line 'could not end a subscription at the provider' names the account and subscription"
    );
    describe_gauge!(
        "uptimepage_subscriptions",
        "Accounts per subscription status (`status` = none | active | past_due | canceled). `past_due` is the number of customers inside their grace window right now"
    );
    describe_counter!(
        "uptimepage_email_admission_total",
        "Addresses the email-admission gate acted on, labelled by `surface`, `outcome` (flagged | refused), and `risk` (disposable | no_mx). Only acted-on addresses are counted, so this is a rate to alert on, not a funnel: a clean address increments nothing. `refused` on a signup surface rising sharply is the shape of scripted abuse; a slow trickle of `flagged` is ordinary"
    );
}

pub mod names {
    pub const CHECKS_TOTAL: &str = "uptimepage_checks_total";
    pub const CHECK_ERRORS: &str = "uptimepage_checks_errors_total";
    pub const CHECK_REDIRECTS: &str = "uptimepage_check_redirects_total";
    pub const HTTP_ACCESS_DIAGNOSTICS: &str = "uptimepage_http_access_diagnostics_total";
    pub const BREAKER_STATE_CHANGES: &str = "uptimepage_circuit_breaker_state_changes_total";
    pub const STORAGE_WRITES: &str = "uptimepage_storage_writes_total";
    pub const STORAGE_DROPPED: &str = "uptimepage_storage_dropped_results_total";
    pub const NOTIFICATIONS_DEAD_LETTERED: &str = "uptimepage_notifications_dead_lettered_total";
    pub const NOTIFICATION_DELIVERY_MS: &str = "uptimepage_notification_delivery_ms";
    pub const CHANNELS_FAILING: &str = "uptimepage_channels_failing";
    pub const ALERTS_DAMPED: &str = "uptimepage_alerts_damped_total";
    pub const MONITORS_UNMONITORED: &str = "uptimepage_monitors_unmonitored";
    pub const TELEGRAM_SEND_DEFERRED: &str = "uptimepage_telegram_send_deferred_total";
    pub const TELEGRAM_SEND_WAIT_MS: &str = "uptimepage_telegram_send_wait_ms";
    pub const CHECK_DURATION_MS: &str = "uptimepage_check_duration_ms";
    pub const CHECK_DNS_MS: &str = "uptimepage_check_dns_ms";
    pub const CHECK_CONNECT_MS: &str = "uptimepage_check_connect_ms";
    pub const CHECK_TLS_MS: &str = "uptimepage_check_tls_ms";
    pub const CHECK_TTFB_MS: &str = "uptimepage_check_ttfb_ms";
    pub const STORAGE_BATCH_SIZE: &str = "uptimepage_storage_batch_size";
    pub const STORAGE_WRITE_DURATION_MS: &str = "uptimepage_storage_write_duration_ms";
    pub const TARGETS_TOTAL: &str = "uptimepage_targets_total";
    pub const TARGETS_ENABLED: &str = "uptimepage_targets_enabled";
    pub const USERS_ACTIVE: &str = "uptimepage_users_active";
    pub const NOTIFICATION_CHANNELS: &str = "uptimepage_notification_channels";
    pub const NOTIFICATION_CHANNEL_ORGS: &str = "uptimepage_notification_channel_orgs";
    pub const ORGS_WITH_CHANNELS: &str = "uptimepage_orgs_with_channels";
    pub const WORKERS_IN_FLIGHT: &str = "uptimepage_workers_in_flight";
    pub const RESULT_QUEUE_DEPTH: &str = "uptimepage_result_queue_depth";
    pub const BREAKERS_OPEN: &str = "uptimepage_circuit_breakers_open";
    pub const NOTIFICATIONS_TOTAL: &str = "uptimepage_notifications_total";
    pub const NOTIFICATIONS_FAILURES: &str = "uptimepage_notifications_failures_total";
    pub const ALERTS_DROPPED: &str = "uptimepage_alerts_dropped_total";
    pub const ALERTS_HELD_MAINTENANCE: &str = "uptimepage_alerts_held_maintenance_total";
    pub const HOST_THROTTLE_WAITS: &str = "uptimepage_host_throttle_waits_total";
    pub const HOST_THROTTLE_DROPS: &str = "uptimepage_host_throttle_drops_total";
    pub const RDAP_SINGLEFLIGHT: &str = "uptimepage_rdap_singleflight_total";
    pub const DOMAIN_EXPIRY_STALE_SERVED: &str = "uptimepage_domain_expiry_stale_served_total";
    pub const DOMAIN_EXPIRY_STATE_WRITE_FAILED: &str =
        "uptimepage_domain_expiry_state_write_failed_total";
    pub const RDAP_SINGLEFLIGHT_SLOTS: &str = "uptimepage_rdap_singleflight_slots";
    pub const AGENT_LAST_SEEN_AGE: &str = "uptimepage_agent_last_seen_age_seconds";
    pub const AGENT_UP: &str = "uptimepage_agent_up";
    pub const AGENTS_ENABLED_DOWN: &str = "uptimepage_agents_enabled_down";
    pub const REGION_AGENTS_TOTAL: &str = "uptimepage_region_agents_total";
    pub const REGION_AGENTS_UP: &str = "uptimepage_region_agents_up";
    pub const REGION_CHECKS_WINDOW: &str = "uptimepage_region_checks_window";
    pub const REGION_CHECKS_UP_WINDOW: &str = "uptimepage_region_checks_up_window";
    pub const REGION_CHECK_LATENCY_P95_MS: &str = "uptimepage_region_check_latency_p95_ms";
    pub const CHECK_ERROR_CLASS_CHECKS: &str = "uptimepage_check_error_class_checks";
    pub const CHECK_ERROR_CLASS_TOP_MONITOR_SHARE: &str =
        "uptimepage_check_error_class_top_monitor_share";
    pub const CHECK_ERROR_CLASS_SWEEP_AGE: &str = "uptimepage_check_error_class_sweep_age_seconds";
    pub const CHECK_ERROR_CLASS_TRUNCATED: &str = "uptimepage_check_error_class_truncated";
    pub const SCHEDULER_REFRESH_FAILED: &str = "uptimepage_scheduler_refresh_failed_total";
    pub const SCHEDULER_CONSECUTIVE_REFRESH_FAILURES: &str =
        "uptimepage_scheduler_consecutive_refresh_failures";
    pub const SCHEDULER_REFRESH_DURATION_MS: &str = "uptimepage_scheduler_refresh_duration_ms";
    pub const PG_POOL_SIZE: &str = "uptimepage_pg_pool_size";
    pub const PG_POOL_IDLE: &str = "uptimepage_pg_pool_idle";
    pub const PG_POOL_IN_USE: &str = "uptimepage_pg_pool_in_use";
    pub const PROCESS_RESIDENT_BYTES: &str = "uptimepage_process_resident_bytes";
    pub const PROCESS_START_TIME: &str = "uptimepage_process_start_time_seconds";
    pub const CLICKHOUSE_MAX_PART_COUNT: &str =
        "uptimepage_clickhouse_max_part_count_for_partition";
    pub const ACCOUNT_DELETIONS_REQUESTED: &str = "uptimepage_account_deletions_requested_total";
    pub const BILLING_WEBHOOKS: &str = "uptimepage_billing_webhooks_total";
    pub const BILLING_WEBHOOK_REJECTED: &str = "uptimepage_billing_webhook_rejected_total";
    pub const BILLING_PROVIDER_CANCEL_FAILED: &str =
        "uptimepage_billing_provider_cancel_failed_total";
    pub const SUBSCRIPTIONS: &str = "uptimepage_subscriptions";
    /// Labelled `action` (linked/unlinked) + `origin` (signup/email_match/session).
    /// A rise in `linked`+`email_match` without matching sign-ups is what a
    /// provider attesting addresses it should not looks like.
    pub const CREDENTIAL_CHANGES: &str = "uptimepage_credential_changes_total";
    /// A link callback whose state named a user the live session was not,
    /// labelled `reason`. `no_session` is routine (the session lapsed on the
    /// provider's consent screen); `other_user` and `identity_taken` should sit
    /// at zero.
    pub const CREDENTIAL_LINK_REFUSED: &str = "uptimepage_credential_link_refused_total";
    /// A passkey sign-in that got as far as a verified challenge and was still
    /// refused, labelled `reason`. Every reason means the assertion resolved to
    /// something this deployment cannot back, so none should be routine.
    pub const PASSKEY_LOGIN_REFUSED: &str = "uptimepage_passkey_login_refused_total";
    /// A hardware authenticator whose signature counter did not advance. The
    /// spec calls this a possible clone. Synced passkeys carry no counter, so
    /// they never reach this; anything here is worth looking at.
    pub const PASSKEY_COUNTER_STALLED: &str = "uptimepage_passkey_counter_stalled_total";
    pub const ORGS_EMPTIED: &str = "uptimepage_orgs_emptied_total";
    pub const HTTP_REQUESTS_TOTAL: &str = "uptimepage_http_requests_total";
    pub const HTTP_REQUEST_DURATION_MS: &str = "uptimepage_http_request_duration_ms";
    pub const HTTP_RESPONSES_INFLIGHT: &str = "uptimepage_http_responses_inflight";
    pub const RATELIMIT_DROPS: &str = "uptimepage_ratelimit_drops_total";
    pub const FLOW_RUNS: &str = "uptimepage_flow_runs_total";
    pub const FLOW_STEP_DURATION_MS: &str = "uptimepage_flow_step_duration_ms";
    pub const AI_CRAWLER_REQUESTS: &str = "uptimepage_ai_crawler_requests_total";
    pub const DISPOSABLE_CORPUS_DOMAINS: &str = "uptimepage_disposable_corpus_domains";
    /// Unix seconds of the last refresh that replaced the corpus. Staleness is
    /// `time() - value`; a gauge holding an age would be wrong between the
    /// hours-apart refreshes that set it.
    pub const DISPOSABLE_CORPUS_UPDATED: &str =
        "uptimepage_disposable_corpus_updated_timestamp_seconds";
    /// Labelled `surface` + `outcome` (flagged | refused) + `risk`. Counts only
    /// addresses the gate acted on; a clean address increments nothing.
    pub const EMAIL_ADMISSION: &str = "uptimepage_email_admission_total";
}
