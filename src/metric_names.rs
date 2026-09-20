//! Every metric this process emits, by name. A leaf so any module can
//! record one without pulling in what observability itself watches.

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
pub const CLICKHOUSE_MAX_PART_COUNT: &str = "uptimepage_clickhouse_max_part_count_for_partition";
pub const ACCOUNT_DELETIONS_REQUESTED: &str = "uptimepage_account_deletions_requested_total";
pub const BILLING_WEBHOOKS: &str = "uptimepage_billing_webhooks_total";
pub const BILLING_WEBHOOK_REJECTED: &str = "uptimepage_billing_webhook_rejected_total";
pub const BILLING_PROVIDER_CANCEL_FAILED: &str = "uptimepage_billing_provider_cancel_failed_total";
pub const BILLING_PROVIDER_DATE_FAILED: &str = "uptimepage_billing_provider_date_failed_total";
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
