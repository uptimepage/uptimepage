use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use opentelemetry::trace::TraceContextExt;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uptimepage::{
    app::AppState,
    config::AppConfig,
    error::{AppError, Result},
    http_client::client::build_clients,
    jobs::{
        self,
        periodic::{run_purge_loop, run_purge_loop_from_boot},
        retention,
    },
    marketing, observability,
    pipeline::{BatcherConfig, ResultBatcher},
    public_status::{
        AggregatorConfig, IncidentWriter, IncidentWriterConfig, OrgAggregator, OrgPublicSource,
        PageCache, PgIncidentStore, PublicSource,
    },
    quotas, request,
    request::is_health_path,
    scheduler::{self, Scheduler, TargetRegistry},
    storage::{
        self, ClickhouseFlowRunSink, ClickhouseHeartbeatPingSink, ClickhouseResultSink,
        ClickhouseResultsStore, IncidentNarrationStore, MaintenanceStore, NotificationChannelStore,
        PgIncidentNarrationStore, PgMaintenanceStore, PgNotificationChannelStore,
        PostgresTargetStore, ResultSink, ResultsStore, TargetStore, admin::AdminRepo,
    },
    worker::{ResultFanout, WorkerPool},
};

const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(10);

/// One INFO line per HTTP request for ops / Loki — method, path,
/// status, latency, trace_id. Emitted at INFO and NOT coupled to the
/// `http.request` span's level: it fires whenever the global filter
/// admits INFO, regardless of whether that DEBUG span exists. Layered
/// *inside* the span solely so the request's OTLP `trace_id` is in
/// scope — emitting it on this line makes it the Loki→Tempo join key
/// (a Loki derived field on `trace_id` pivots straight to the trace).
///
/// Caveat: `trace_id` is present only when the `http.request` span is
/// actually recorded — i.e. `uptimepage` at DEBUG in the active
/// filter. Under a bare `info` `RUST_LOG` the span is absent and
/// `trace_id` is simply omitted (never a fake id) — the access line
/// still logs, but the Loki→Tempo pivot silently goes dark. The deploy
/// `RUST_LOG` variable must keep `uptimepage=debug`.
async fn access_log(req: Request, next: Next) -> Response {
    // Decide skip from the BORROWED path; only own method+path when
    // we will actually log. Caddy active-health + the deploy gate poll
    // the health endpoints forever — never allocate a String for them.
    let p = req.uri().path();
    let logged = (!is_health_path(p)).then(|| {
        // Path only, never the query string: /auth/* carries
        // single-use magic-link tokens and the OAuth code/state,
        // which must not reach stdout logs. The /m/ share token lives in
        // the path itself, so scrub that segment too.
        (
            req.method().clone(),
            scrub_capability_token(req.uri().path()).into_owned(),
        )
    });
    let start = Instant::now();
    let resp = next.run(req).await;
    if let Some((method, path)) = logged {
        // The enclosing tower_http span carries the exported OTLP
        // context — valid only when that span was recorded (see the
        // caveat above); otherwise the field is omitted, never faked.
        let ctx = tracing::Span::current().context();
        let span = ctx.span();
        let span_ctx = span.span_context();
        let trace_id = span_ctx.is_valid().then(|| span_ctx.trace_id().to_string());
        tracing::info!(
            method = %method,
            path = %path,
            status = resp.status().as_u16(),
            latency_ms = start.elapsed().as_millis() as u64,
            trace_id = trace_id.as_deref(),
            "http access"
        );
    }
    resp
}

/// Replace a path-segment capability token (`/m/{token}` share links,
/// `/ping/{token}` heartbeat pings) with a placeholder so the secret never
/// reaches stdout logs or the exported span. It's a path segment, not a query
/// param, so the access path's query-stripping would miss it.
fn scrub_capability_token(path: &str) -> std::borrow::Cow<'_, str> {
    for prefix in ["/m/", "/ping/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let tail = rest
                .split_once('/')
                .map(|(_, t)| format!("/{t}"))
                .unwrap_or_default();
            return std::borrow::Cow::Owned(format!("{prefix}{{token}}{tail}"));
        }
    }
    std::borrow::Cow::Borrowed(path)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = AppConfig::load()?;
    // Inconsistent trace-export config (missing endpoint/credentials,
    // out-of-range sample ratio) is a clean startup error — validated
    // BEFORE tracing::init builds the exporter, so a bad value never
    // reaches the sampler/transport.
    cfg.validate_observability()?;
    let tracing_guard = observability::tracing::init(&cfg.observability);

    // Operator subcommands run against Postgres only, then exit — before any
    // server-only validation or the brain/agent split.
    if std::env::args().nth(1).as_deref() == Some("bootstrap-owner") {
        // Seeds an owner and a full-access token in PG, so it needs the same guard.
        cfg.validate_storage()?;
        let args = uptimepage::bootstrap::parse_args(std::env::args().skip(2))?;
        let result = uptimepage::bootstrap::run_owner(&cfg, &args).await;
        drop(tracing_guard);
        return result;
    }

    cfg.assert_per_org_status();
    cfg.assert_mcp_oauth();
    // A bad quota/rate/interval number is a clean startup config error,
    // never a `.expect()` crash-loop in router/layer construction (I6).
    cfg.validate_quotas_and_limits()?;
    // Same contract for the abuse rules: a malformed URL-pattern regex or
    // deny-list YAML fails fast here, not as a runtime panic.
    uptimepage::security::AbuseGuard::validate(&cfg.abuse)?;
    // Same for the email-admission knobs: an unrecognised signup_policy
    // would otherwise degrade to "let everything through" in silence.
    cfg.validate_email_policy()?;
    // Marketing host/URL/cookie invariants. Skipped wholesale when
    // marketing.enabled = false (the default).
    cfg.validate_marketing()?;
    // Central Telegram bot: a set bot_token without a username / strong
    // webhook secret / https base is a clean startup error, not a half-up bot.
    cfg.validate_telegram()?;
    cfg.validate_billing()?;
    // Transactional mail: provider = "resend" without key/sender fails here,
    // not on the first verification mail.
    cfg.validate_email()?;
    cfg.validate_microsoft_oauth()?;
    cfg.validate_gitlab_oauth()?;
    // Operator WhatsApp number: a flipped flag with missing creds fails
    // here, not as a dead webhook or a broken first send.
    cfg.validate_whatsapp_app()?;

    let metrics_handle = if cfg.observability.metrics_enabled {
        Some(observability::metrics::init(&cfg.server.metrics_bind)?)
    } else {
        None
    };

    cfg.validate_runtime()?;

    // Regional-agent mode: a stateless probe, no web/PG/CH/alerting. Branches
    // before any of that is constructed.
    if cfg.agent.enabled {
        return uptimepage::agent::run(cfg).await;
    }

    // Agents open no PG/CH, so the guard runs after the agent branch.
    cfg.validate_storage()?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        api_bind = %cfg.server.api_bind,
        metrics_bind = %cfg.server.metrics_bind,
        "starting uptimepage"
    );

    let api_bind = cfg.server.api_bind.clone();
    let cipher = uptimepage::security::Cipher::from_config(&cfg.security)?;
    if cipher.is_none() {
        tracing::warn!(
            "credentials_kek_base64 unset — basic_auth/bearer_token will be stored in plaintext"
        );
    }
    let pg_pool = PostgresTargetStore::connect_pool(&cfg.storage.postgres).await?;
    uptimepage::bootstrap::seed_first_owner(&pg_pool, &cfg).await?;
    storage::admin::AdminRepo::new(pg_pool.clone(), cipher.clone(), "region_reconcile")
        .reconcile_regions(
            &cfg.scheduler.region,
            cfg.scheduler.effective_default_region(),
        )
        .await?;
    let target_store: Arc<dyn TargetStore> = Arc::new(
        PostgresTargetStore::from_pool(pg_pool.clone(), cipher.clone())
            .with_default_region(cfg.scheduler.effective_default_region()),
    );

    tracing::info!(
        // SAFE: operator infra endpoint (no credentials — password is a
        // separate SecretString field), not user data
        url = %cfg.storage.clickhouse.url,
        database = %cfg.storage.clickhouse.database,
        "connecting to clickhouse"
    );
    let clickhouse_client = storage::build_client(&cfg.storage.clickhouse);
    storage::migrate(&clickhouse_client).await?;
    // The control plane's in-process scheduler stamps its own region but a
    // distinct agent id, so the `agent_id` dimension cleanly separates
    // control-plane self-checks from real per-region agents (which carry their
    // agents-row id).
    // Snapshot warms via the refresh task's immediate first tick (spawned
    // below), so boot doesn't block on a PG round-trip; early rows default
    // until it lands.
    let org_ttl = storage::OrgTtlDays::new();
    let result_sink: Arc<dyn ResultSink> = Arc::new(ClickhouseResultSink::new(
        clickhouse_client.clone(),
        cfg.scheduler.region.clone(),
        "control-plane".to_string(),
        org_ttl.clone(),
    ));
    let result_sink_for_state = result_sink.clone();
    // Flow runs bypass the result batcher: at the 300s interval floor they
    // arrive far too rarely to be worth buffering. Wrapped so no path can store
    // a page snapshot that still holds one of the org's secrets.
    let flow_run_sink: Arc<dyn uptimepage::storage::traits::FlowRunSink> =
        Arc::new(uptimepage::storage::ScrubbedFlowRunSink::new(
            Arc::new(ClickhouseFlowRunSink::new(
                clickhouse_client.clone(),
                cfg.scheduler.region.clone(),
                org_ttl.clone(),
            )),
            Arc::new(uptimepage::storage::PgVariableStore::new(
                pg_pool.clone(),
                cipher.clone(),
            )),
        ));
    let flow_run_sink_for_state = flow_run_sink.clone();
    let heartbeat_ping_sink: Arc<dyn uptimepage::storage::traits::HeartbeatPingSink> = Arc::new(
        ClickhouseHeartbeatPingSink::new(clickhouse_client.clone(), org_ttl.clone()),
    );
    let ch_client_for_public = clickhouse_client.clone();
    let ch_client_for_purge = clickhouse_client.clone();
    let ch_client_for_sampler = clickhouse_client.clone();
    let ch_client_for_region = clickhouse_client.clone();
    let ch_client_for_error_classes = clickhouse_client.clone();
    let results_store: Arc<dyn ResultsStore> =
        Arc::new(ClickhouseResultsStore::from_client(clickhouse_client));

    let http_clients = Arc::new(build_clients(
        &cfg.http_client,
        &cfg.checker,
        &cfg.dns,
        &cfg.security,
    )?);

    let (result_tx, result_rx) = mpsc::channel(cfg.storage.clickhouse.buffer_size.max(1024));
    // Notification channels are per-org and edited at runtime, so the alert
    // path is always wired (no global enable gate). The engine resolves a
    // target's bound channels from this store per result.
    let notification_channel_store: Arc<dyn NotificationChannelStore> = Arc::new(
        PgNotificationChannelStore::new(pg_pool.clone(), cipher.clone()),
    );
    let fanout = ResultFanout::new(result_tx.clone());
    // Incident-driven paging is the single notification path: the writer opens
    // region-aware incidents and the engine delivers them — to a monitor's bound
    // channels (simple mode) or up an escalation policy when one is bound.
    let (incident_signal_tx, incident_signal_rx) =
        mpsc::channel::<uptimepage::escalation::IncidentSignal>(1024);
    let host_throttle = Arc::new(uptimepage::worker::host_throttle::HostThrottle::new(
        cfg.checker.per_host_max_inflight,
        cfg.checker.rdap_max_inflight,
    ));
    let rdap_client = Arc::new(uptimepage::worker::rdap::RdapClient::new(
        uptimepage::http_outbound::build_outbound_client(uptimepage::security::SsrfGuard::strict()),
    ));
    let domain_expiry_state: Arc<dyn uptimepage::storage::DomainExpiryStateStore> = Arc::new(
        uptimepage::storage::PgDomainExpiryStateStore::new(pg_pool.clone()),
    );
    let domain_expiry_runtime =
        Arc::new(uptimepage::worker::domain_expiry::DomainExpiryRuntime::new(
            Arc::new(uptimepage::worker::registration::RegistrationClient::new(
                rdap_client,
            )),
            Arc::new(uptimepage::worker::rdap_singleflight::RdapSingleflight::with_default_ttl()),
            domain_expiry_state,
            host_throttle.clone(),
            uptimepage::worker::domain_expiry::DEFAULT_MAX_STALENESS,
        ));
    let flow_engine = cfg.flow.enabled.then(|| {
        Arc::new(uptimepage::worker::flow::engine::CdpEngine::new(
            uptimepage::worker::flow::engine::FlowEngineConfig {
                binary: cfg.flow.lightpanda_path.clone().into(),
                max_concurrency: cfg.flow.max_concurrency,
                mem_limit_bytes: cfg.flow.mem_limit_mb.saturating_mul(1024 * 1024),
                block_private_networks: cfg.flow.block_private_networks,
                block_cidrs: cfg.flow.block_cidrs.clone(),
                v8_max_heap_mb: cfg.flow.v8_max_heap_mb,
                max_response_bytes: cfg.flow.max_response_mb.saturating_mul(1024 * 1024),
                user_agent_suffix: cfg.flow.user_agent_suffix.clone(),
            },
        ))
    });
    let pool = Arc::new(
        WorkerPool::new(
            cfg.checker.max_concurrent_checks,
            (*http_clients).clone(),
            cfg.circuit_breaker,
            fanout,
            host_throttle.clone(),
            domain_expiry_runtime,
        )
        .with_flow_engine(flow_engine)
        .with_flow_runs(Some(flow_run_sink)),
    );
    // Shared with AppState: a second instance would hold its own plan cache.
    let quotas = Arc::new(quotas::QuotaService::new(&cfg, Some(pg_pool.clone())));

    // With in-process probing disabled the scheduler still runs, fed only the
    // passive heartbeat set (agents can't evaluate that state). Both sources
    // reconcile the anchor cache on every refresh before dispatching.
    let scheduler_source: Arc<dyn storage::admin::EnabledTargetSource> = if cfg.scheduler.enabled {
        Arc::new(scheduler::sources::RegionTargetSource::new(
            storage::admin::AdminRepo::new(pg_pool.clone(), cipher.clone(), "scheduler_refresh"),
            cfg.scheduler.region.clone(),
            pool.heartbeat_runtime(),
            Arc::clone(&quotas),
            cfg.flow.enabled,
        ))
    } else {
        Arc::new(scheduler::sources::HeartbeatTargetSource::new(
            storage::admin::AdminRepo::new(pg_pool.clone(), cipher.clone(), "heartbeat_refresh"),
            pool.heartbeat_runtime(),
        ))
    };
    let registry = Arc::new(TargetRegistry::new(scheduler_source));

    let batcher_cfg = BatcherConfig {
        batch_size: cfg.storage.clickhouse.batch_size.max(1),
        batch_timeout: Duration::from_millis(cfg.storage.clickhouse.batch_timeout_ms),
    };
    let batcher = ResultBatcher::new(result_rx, result_sink, batcher_cfg);

    let root = CancellationToken::new();
    // Always spawned: with probing off the source above narrows the schedule
    // to heartbeat evaluation only.
    if !cfg.scheduler.enabled {
        tracing::info!("in-process probing disabled; scheduler evaluates heartbeats only");
    }
    let scheduler_handle: JoinHandle<()> = {
        let scheduler = Arc::new(Scheduler::new(
            registry.clone(),
            pool.clone(),
            cfg.scheduler.clone(),
        ));
        let token = root.clone();
        tokio::spawn(async move {
            if let Err(err) = scheduler.run(token).await {
                tracing::error!(?err, "scheduler exited with error");
            }
        })
    };
    let batcher_handle: JoinHandle<()> = {
        let token = root.clone();
        tokio::spawn(async move { batcher.run(token).await })
    };
    let org_ttl_handle: JoinHandle<()> =
        storage::org_ttl::spawn_refresh(org_ttl, pg_pool.clone(), root.clone());
    // Floor at 100ms to keep a misconfigured 0 / sub-tick value from spinning.
    let sample_interval =
        Duration::from_millis(cfg.observability.gauge_sample_interval_ms.max(100));
    let sampler_handle: JoinHandle<()> = uptimepage::scheduler::sampler::spawn(
        pool.clone(),
        registry.clone(),
        Some(uptimepage::scheduler::sampler::DbSources {
            pg_pool: pg_pool.clone(),
            ch: ch_client_for_sampler,
        }),
        &result_tx,
        sample_interval,
        root.clone(),
    );
    drop(result_tx);

    let aggregator_cfg = AggregatorConfig {
        app_base_url: cfg.auth.public_base_url.clone(),
        ..AggregatorConfig::default()
    };
    let aggregator = Arc::new(OrgAggregator::new(
        pg_pool.clone(),
        ch_client_for_public,
        aggregator_cfg.clone(),
        cipher.clone(),
    ));
    let public_cache = PageCache::new(&cfg.public_status);
    let public_source: Arc<dyn PublicSource> = Arc::new(OrgPublicSource::new(
        aggregator,
        public_cache,
        pg_pool.clone(),
        aggregator_cfg.site_name.clone(),
    ));

    let pg_pool_for_stores = pg_pool.clone();
    // Governed like the scheduler: the writer sizes its lookback from the
    // interval, so it must see the rate the monitor actually runs at.
    let admin_repo_for_writer = Arc::new(quotas::PlanGoverned::new(
        Arc::new(AdminRepo::new(
            pg_pool_for_stores.clone(),
            cipher.clone(),
            "incident_writer",
        )),
        Arc::clone(&quotas),
    ));
    let writer = IncidentWriter::new(
        admin_repo_for_writer,
        results_store.clone(),
        Arc::new(PgIncidentStore::new(pg_pool)),
        IncidentWriterConfig::default(),
    )
    .with_signals(incident_signal_tx.clone());
    let incident_writer = Arc::new(writer);
    let incident_writer_handle: JoinHandle<()> = {
        let writer = incident_writer.clone();
        let token = root.clone();
        tokio::spawn(async move { writer.run(token).await })
    };

    // Dead-man's-switch for regional agents: gauges + a warn log when one goes
    // dark. Cheap periodic DB read; harmless in a single-region deployment.
    let agent_health_handle: JoinHandle<()> = {
        let repo = uptimepage::storage::operator::OperatorRepo::new(pg_pool_for_stores.clone());
        let stale_after = std::time::Duration::from_secs(cfg.operator.agent_stale_after_secs);
        let token = root.clone();
        tokio::spawn(async move {
            uptimepage::observability::agent_health::run(repo, stale_after, token).await
        })
    };

    // Inventory gauges from Postgres on a slow cadence — correct on a brain
    // where the scheduler registry is empty because agents do the probing.
    let inventory_handle: JoinHandle<()> = {
        let pg = pg_pool_for_stores.clone();
        let token = root.clone();
        tokio::spawn(async move { uptimepage::observability::inventory::run(pg, token).await })
    };

    // Delivery health of the channels themselves — a dead endpoint nothing has
    // tried to page is invisible to the dispatch counters.
    let channel_health_handle: JoinHandle<()> = {
        let pg = pg_pool_for_stores.clone();
        let limit = cfg.escalation.channel_failure_limit;
        let token = root.clone();
        tokio::spawn(async move {
            uptimepage::observability::channel_health::run(pg, limit, token).await
        })
    };

    // Per-region probe quality from ClickHouse — brain-side, so it covers the
    // remote agents Alloy can't scrape and scales with regions, not customers.
    let region_health_handle: JoinHandle<()> = {
        let ch = ch_client_for_region;
        let token = root.clone();
        tokio::spawn(async move { uptimepage::observability::region_health::run(ch, token).await })
    };

    let error_class_handle: JoinHandle<()> = {
        let ch = ch_client_for_error_classes;
        let token = root.clone();
        tokio::spawn(async move { uptimepage::observability::error_classes::run(ch, token).await })
    };

    // Incident paging worker: the single notification path. Always running — it
    // pages a monitor's bound channels on open/resolve (region-aware) and walks
    // an escalation policy when one is bound. The `escalation.enabled` flag only
    // gates the policy/on-call UI, not whether incidents notify.
    let escalation_policy_store: Arc<dyn uptimepage::storage::EscalationPolicyStore> = Arc::new(
        uptimepage::storage::PgEscalationPolicyStore::new(pg_pool_for_stores.clone()),
    );
    let on_call_store: Arc<dyn uptimepage::storage::OnCallStore> = Arc::new(
        uptimepage::storage::PgOnCallStore::new(pg_pool_for_stores.clone()),
    );
    let contact_store: Arc<dyn uptimepage::storage::ContactStore> = Arc::new(
        uptimepage::storage::PgContactStore::new(pg_pool_for_stores.clone()),
    );
    // One process-wide central-bot send budget, shared by the engine and the
    // web side (test-now, webhook replies) via AppState.
    let telegram_send_budget = std::sync::Arc::new(uptimepage::telegram::TelegramSendBudget::new());
    let outbound_http = uptimepage::http_outbound::build_outbound_client(
        uptimepage::security::SsrfGuard::from_security_config(&cfg.security),
    );
    let email_sender = uptimepage::email::build_email_sender(&cfg.email, &outbound_http)
        .map_err(|e| AppError::Other(anyhow::anyhow!("build_email_sender: {e}")))?;
    let alert_channel_stop_secret = uptimepage::storage::app_secrets::ensure_secret(
        &pg_pool_for_stores,
        cipher.as_deref(),
        "alert_channel_stop",
    )
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("alert-channel stop secret: {e}")))?;
    let incident_ack_secret = uptimepage::storage::app_secrets::ensure_secret(
        &pg_pool_for_stores,
        cipher.as_deref(),
        "incident_ack",
    )
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("incident ack secret: {e}")))?;
    let org_directory: Arc<dyn uptimepage::storage::orgs::OrgDirectory> = Arc::new(
        uptimepage::storage::orgs::PgOrgDirectory::new(pg_pool_for_stores.clone()),
    );
    let maintenance_store: Arc<dyn MaintenanceStore> =
        Arc::new(PgMaintenanceStore::new(pg_pool_for_stores.clone()));
    let escalation_engine_handle: JoinHandle<()> = {
        let engine = uptimepage::escalation::EscalationEngine::new(
            incident_signal_rx,
            uptimepage::escalation::EngineDeps {
                ops: Arc::new(uptimepage::storage::PgIncidentOpsStore::new(
                    pg_pool_for_stores.clone(),
                )),
                policies: escalation_policy_store.clone(),
                on_call: on_call_store.clone(),
                contacts: contact_store.clone(),
                targets: target_store.clone(),
                channels: notification_channel_store.clone(),
                maintenance: maintenance_store.clone(),
                orgs: org_directory.clone(),
                http: outbound_http.clone(),
                cfg: cfg.escalation.clone(),
                base_url: cfg.auth.public_base_url.clone(),
                alert_channel_stop_secret: alert_channel_stop_secret.clone(),
                incident_ack_secret: incident_ack_secret.clone(),
                central_bot: cfg.telegram.enabled().then(|| {
                    uptimepage::notifier::CentralBotDelivery {
                        token: cfg.telegram.bot_token.clone(),
                        budget: telegram_send_budget.clone(),
                    }
                }),
                central_whatsapp: cfg.whatsapp_app.enabled().then(|| cfg.whatsapp_app.clone()),
                email: Some(uptimepage::notifier::EmailDelivery {
                    sender: email_sender.clone(),
                    from_address: cfg.email.from_address.clone(),
                    from_name: cfg.email.from_name.clone(),
                }),
            },
        );
        let token = root.clone();
        tokio::spawn(async move { engine.run(token).await })
    };

    // Per-monitor silence: rolls agent liveness up to the customer's monitors so
    // a dead probe is told to the customer, not just shown as a stale agent gauge.
    let silence_sweep_handle: JoinHandle<()> = {
        let store: Arc<dyn uptimepage::storage::SilenceStore> = Arc::new(
            uptimepage::storage::PgSilenceStore::new(pg_pool_for_stores.clone()),
        );
        let delivery: Arc<dyn uptimepage::jobs::silence::SilenceDelivery> =
            Arc::new(uptimepage::jobs::silence::SilenceNotifier {
                channels: notification_channel_store.clone(),
                targets: target_store.clone(),
                orgs: org_directory.clone(),
                http: outbound_http.clone(),
                central_bot: cfg.telegram.enabled().then(|| {
                    uptimepage::notifier::CentralBotDelivery {
                        token: cfg.telegram.bot_token.clone(),
                        budget: telegram_send_budget.clone(),
                    }
                }),
                central_whatsapp: cfg.whatsapp_app.enabled().then(|| cfg.whatsapp_app.clone()),
                email: Some(uptimepage::notifier::EmailDelivery {
                    sender: email_sender.clone(),
                    from_address: cfg.email.from_address.clone(),
                    from_name: cfg.email.from_name.clone(),
                }),
                base_url: cfg.auth.public_base_url.clone(),
                alert_channel_stop_secret: alert_channel_stop_secret.clone(),
            });
        let stale_after = std::time::Duration::from_secs(cfg.operator.agent_stale_after_secs);
        let token = root.clone();
        let quotas = Arc::clone(&quotas);
        tokio::spawn(async move {
            uptimepage::jobs::silence::run(store, delivery, quotas, stale_after, token).await
        })
    };

    let purge_handle: JoinHandle<()> = {
        let pool = pg_pool_for_stores.clone();
        let token = root.clone();
        let grace_days = cfg.tenancy.deletion_grace_period_days;
        tokio::spawn(retention::run(
            pool,
            ch_client_for_purge,
            cfg.retention,
            cfg.auth.session.clone(),
            grace_days,
            token,
        ))
    };

    let status_page_store: Arc<dyn uptimepage::storage::StatusPageStore> = Arc::new(
        uptimepage::storage::PgStatusPageStore::new(pg_pool_for_stores.clone()),
    );
    let incident_narration_store: Arc<dyn IncidentNarrationStore> =
        Arc::new(PgIncidentNarrationStore::new(pg_pool_for_stores.clone()));

    uptimepage::auth::ensure_fingerprint_salt(&pg_pool_for_stores, &cfg.auth.fingerprint_salt)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("auth salt guard: {e}")))?;

    // Moving the app retires every passkey at once and nothing can migrate
    // them, so the least we owe is not letting it happen quietly.
    if uptimepage::auth::passkey::login_enabled(&cfg.auth)
        && let Ok(rp_id) = uptimepage::auth::passkey::relying_party_id(&cfg.auth.public_base_url)
        && let Ok(orphaned) =
            uptimepage::storage::passkeys::orphaned_by_rp_id(&pg_pool_for_stores, &rp_id).await
        && orphaned > 0
    {
        tracing::warn!(
            rp_id = %rp_id,
            orphaned,
            "auth.public_base_url does not match the host these passkeys were created for; \
             they can no longer sign anyone in and their owners must add new ones"
        );
    }

    let unsubscribe_secret = uptimepage::storage::app_secrets::ensure_secret(
        &pg_pool_for_stores,
        cipher.as_deref(),
        "subscription_unsubscribe",
    )
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("unsubscribe secret: {e}")))?;
    let billing_checkout_secret = uptimepage::storage::app_secrets::ensure_secret(
        &pg_pool_for_stores,
        cipher.as_deref(),
        "billing_checkout",
    )
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("billing checkout secret: {e}")))?;

    let prefix_len = cfg.auth.api_tokens.prefix_visible_chars as usize;
    if prefix_len < uptimepage::auth::api_tokens::MIN_PREFIX_VISIBLE_CHARS {
        return Err(AppError::Other(anyhow::anyhow!(
            "auth.api_tokens.prefix_visible_chars must be >= {} (got {prefix_len})",
            uptimepage::auth::api_tokens::MIN_PREFIX_VISIBLE_CHARS
        )));
    }

    let invitation_purge_handle: JoinHandle<()> = {
        let keep_days = i64::from(cfg.auth.invitations.expiry_hours / 24).max(1);
        tokio::spawn(run_purge_loop(
            pg_pool_for_stores.clone(),
            root.clone(),
            Duration::from_secs(6 * 60 * 60),
            "invitations_cleanup",
            async move |pool: &sqlx::PgPool| {
                uptimepage::auth::invitations::purge_old(pool, keep_days).await
            },
        ))
    };

    // 10 min: oauth_states rows carry a 10-minute TTL, so abandoned dances
    // are swept roughly as fast as they expire.
    let oauth_state_cleanup_handle: JoinHandle<()> = tokio::spawn(run_purge_loop(
        pg_pool_for_stores.clone(),
        root.clone(),
        Duration::from_secs(10 * 60),
        "oauth_state_cleanup",
        uptimepage::auth::oauth_state::purge_expired,
    ));

    // Matches the ceremony TTL, so an abandoned one is swept about as fast as
    // it expires. Consumed rows delete themselves as they are read.
    let webauthn_state_cleanup_handle: JoinHandle<()> = tokio::spawn(run_purge_loop(
        pg_pool_for_stores.clone(),
        root.clone(),
        Duration::from_secs(5 * 60),
        "webauthn_state_cleanup",
        uptimepage::storage::passkeys::purge_expired,
    ));

    let channel_verification_cleanup_handle: JoinHandle<()> = tokio::spawn(run_purge_loop(
        pg_pool_for_stores.clone(),
        root.clone(),
        Duration::from_secs(6 * 60 * 60),
        "channel_verification_cleanup",
        uptimepage::storage::channel_verification::purge_old,
    ));

    let subscriber_dispatch_handle: JoinHandle<()> = {
        let dispatcher = uptimepage::public_status::subscriber_dispatch::SubscriberDispatcher::new(
            pg_pool_for_stores.clone(),
            email_sender.clone(),
            outbound_http.clone(),
            uptimepage::public_status::subscriber_dispatch::SubscriberDispatchConfig {
                tick_interval: Duration::from_secs(20),
                batch_limit: 200,
                base_domain: cfg.public_status.base_domain.clone(),
                public_base_url: cfg.auth.public_base_url.clone(),
                subdomain_routes: cfg.tenancy.subdomain_public_routes,
                unsubscribe_secret: unsubscribe_secret.clone(),
                from_address: cfg.email.from_address.clone(),
                from_name: cfg.email.from_name.clone(),
            },
        );
        let token = root.clone();
        tokio::spawn(async move { dispatcher.run(token).await })
    };

    let subscriber_token_cleanup_handle: JoinHandle<()> = tokio::spawn(run_purge_loop(
        pg_pool_for_stores.clone(),
        root.clone(),
        Duration::from_secs(6 * 60 * 60),
        "subscriber_token_cleanup",
        uptimepage::storage::subscribers::purge_old_tokens,
    ));

    let subscriber_delivery_cleanup_handle: JoinHandle<()> = tokio::spawn(run_purge_loop(
        pg_pool_for_stores.clone(),
        root.clone(),
        Duration::from_secs(6 * 60 * 60),
        "subscriber_delivery_cleanup",
        uptimepage::storage::subscriber_deliveries::purge_old,
    ));

    // A plan moves by an operator UPDATE, which notifies nothing, so this turns
    // a downgrade into holds and an upgrade into their release. It runs at boot
    // too, because the plan can have moved while the process was down. The
    // candidate query skips every account that already fits.
    let plan_holds_handle: JoinHandle<()> = {
        let quotas = Arc::clone(&quotas);
        tokio::spawn(run_purge_loop_from_boot(
            pg_pool_for_stores.clone(),
            root.clone(),
            Duration::from_secs(24 * 60 * 60),
            "plan_holds",
            async move |pool: &sqlx::PgPool| uptimepage::quotas::holds::sweep(pool, &quotas).await,
        ))
    };

    let heartbeat_prev_token_cleanup_handle: JoinHandle<()> = tokio::spawn(run_purge_loop(
        pg_pool_for_stores.clone(),
        root.clone(),
        Duration::from_secs(6 * 60 * 60),
        "heartbeat_prev_token_cleanup",
        uptimepage::storage::heartbeats::purge_expired_prev_tokens,
    ));

    // 6h, not daily: the three-day age is the real gate, and the tick only
    // bounds how late the reminder lands.
    let heartbeat_nudge_handle: JoinHandle<()> = {
        let nudge = Arc::new(uptimepage::jobs::heartbeat_nudge::NudgeConfig {
            email: uptimepage::notifier::EmailDelivery {
                sender: email_sender.clone(),
                from_address: cfg.email.from_address.clone(),
                from_name: cfg.email.from_name.clone(),
            },
            public_base_url: cfg.auth.public_base_url.clone(),
            docs_url: Some(uptimepage::templates::filters::docs_link("/monitor-types")),
        });
        tokio::spawn(run_purge_loop(
            pg_pool_for_stores.clone(),
            root.clone(),
            Duration::from_secs(6 * 60 * 60),
            "heartbeat_nudge",
            async move |pool: &sqlx::PgPool| {
                uptimepage::jobs::heartbeat_nudge::nudge_unwired_heartbeats(pool, &nudge).await
            },
        ))
    };

    // Magic-link sweep only runs when the method is wired into the router.
    // When disabled the routes 404, no rows are ever inserted, and the ticker
    // would be dead weight.
    let magic_link_cleanup_handle: Option<JoinHandle<()>> =
        cfg.auth.magic_link_enabled().then(|| {
            tokio::spawn(run_purge_loop(
                pg_pool_for_stores.clone(),
                root.clone(),
                Duration::from_secs(6 * 60 * 60),
                "magic_link_cleanup",
                uptimepage::auth::magic_link::purge_old,
            ))
        });

    let state = AppState::new(
        cfg,
        Some(pg_pool_for_stores),
        target_store,
        results_store,
        result_sink_for_state,
        http_clients.clone(),
        pool.clone(),
        public_source,
        maintenance_store,
        notification_channel_store,
        status_page_store,
        incident_narration_store,
        outbound_http,
        email_sender,
        cipher,
        quotas,
    );
    let state = state
        .with_flow_run_sink(flow_run_sink_for_state)
        .with_heartbeat_ping_sink(heartbeat_ping_sink)
        .with_telegram_send_budget(telegram_send_budget)
        .with_incident_signals(incident_signal_tx)
        .with_subscription_unsubscribe_secret(unsubscribe_secret)
        .with_alert_channel_stop_secret(alert_channel_stop_secret)
        .with_incident_ack_secret(incident_ack_secret)
        .with_billing_checkout_secret(billing_checkout_secret)
        .with_shutdown(root.clone());
    let billing_for_drain = state.billing.clone();
    // Scheduled plan changes, grace expiries and payment reminders run on our
    // clock, not the provider's. From boot, since a deadline can pass while
    // the process is down.
    let billing_sweep_handle: Option<JoinHandle<()>> = state
        .billing
        .clone()
        .zip(state.db.clone())
        .map(|(billing, pool)| {
            let quotas = Arc::clone(&state.quotas);
            tokio::spawn(run_purge_loop_from_boot(
                pool,
                root.clone(),
                Duration::from_secs(15 * 60),
                "billing_sweep",
                async move |pool: &sqlx::PgPool| billing.sweep(pool, &quotas).await,
            ))
        });
    // Hot-reload the abuse deny-lists on SIGHUP when enabled (validate then
    // atomic swap; a bad edit is rejected and the running rules stay).
    let abuse_reload_handle: Option<JoinHandle<()>> = uptimepage::security::abuse_reload::spawn(
        state.abuse.clone(),
        state.cfg.clone(),
        root.clone(),
    );

    // Fill the disposable-email corpus before the listener binds, so the first
    // signup after a restart is judged by the same set as the last one before
    // it. The refresh job then keeps it current on its own cadence.
    if let Some(pool) = state.db.as_ref() {
        uptimepage::jobs::disposable_refresh::load_persisted(pool, &state.email_policy).await;
    }
    let disposable_refresh_handle: Option<JoinHandle<()>> = state.db.as_ref().and_then(|pool| {
        uptimepage::jobs::disposable_refresh::spawn(
            pool.clone(),
            state.outbound_http.clone(),
            Arc::new(state.cfg.email_policy.clone()),
            state.email_policy.clone(),
            root.clone(),
        )
    });

    let snitch_handle: Option<JoinHandle<()>> = jobs::snitch::spawn(
        &state.cfg.observability.heartbeat,
        state.outbound_http.clone(),
        state.target_store.clone(),
        state.results_store.clone(),
        root.clone(),
    );

    // All-in-one mode: when this process probes its own region in-process
    // (scheduler enabled), also serve interactive test/check-now locally so a
    // self-hosted single process needs no separate agent. A pure control plane
    // (scheduler disabled) leaves this to its regional agents.
    let local_dispatch_handle: Option<JoinHandle<()>> = if state.cfg.scheduler.enabled {
        Some(tokio::spawn(
            uptimepage::ad_hoc_dispatch::run_local_executor(
                state.ad_hoc.clone(),
                state.cfg.scheduler.region.clone(),
                state.worker_pool.clone(),
                state.http_clients.clone(),
                state.result_sink.clone(),
                root.clone(),
            ),
        ))
    } else {
        None
    };

    // Register the central-bot webhook in the background so a Telegram outage
    // never stalls the boot; the next boot re-asserts it idempotently.
    if state.cfg.telegram.enabled() {
        let cfg = state.cfg.clone();
        let http = state.outbound_http.clone();
        tokio::spawn(async move { uptimepage::telegram::ensure_webhook(&cfg, http).await });
    }

    // One span per HTTP request — the unit the OTLP layer exports; with
    // no instrumented span there is nothing to trace. DEBUG level so the
    // span is recorded only when the filter is at least debug.
    //
    // Layer order is load-bearing: with chained `Router::layer`, the
    // LAST `.layer` is OUTERMOST. TraceLayer added last → it wraps and
    // enters the `http.request` span before `access_log` runs, so
    // `access_log` can read the request's OTLP trace_id (see its doc).
    // Default-deny on tenant subdomain hosts: only the public-tenant
    // allow-list (`/`, `/status*`, `/api/public/v1/*`, `/static/*`,
    // security.txt) reaches a handler; everything else 404s before the
    // operator UI, auth flows, and private API can leak surface on
    // `{slug}.{base}`. No-op when SaaS subdomain mode is off.
    let app_router = uptimepage::build_app_router(state.clone(), root.clone());

    // The single dispatch seam. When marketing is enabled, requests are
    // routed to the marketing or app router by classified `Host`.
    // Otherwise the app router serves everything as before.
    let combined: Router = if state.cfg.marketing.enabled {
        let scheme = uptimepage::request::host::HostScheme::from_base_domain(
            &state.cfg.public_status.base_domain,
        )
        .map_err(|e| AppError::Other(anyhow::anyhow!("HostScheme: {e}")))?;
        // A provider with an empty catalog would send pricing-page visitors to
        // a billing page with nothing to buy.
        let checkout_open = match (&state.billing, &state.db) {
            (Some(billing), Some(pool)) => {
                !storage::subscriptions::priced_plans(pool, billing.provider.name())
                    .await?
                    .is_empty()
            }
            _ => false,
        };
        let marketing_cfg = marketing::MarketingCfg {
            app_url: state.cfg.marketing.app_url.clone(),
            canonical_origin: state.cfg.marketing.canonical_origin.clone(),
            blog_enabled: state.cfg.marketing.blog_enabled,
            mcp_url: (state.cfg.mcp.enabled && !state.cfg.mcp.resource_uri.is_empty())
                .then(|| state.cfg.mcp.resource_uri.clone()),
            checkout_open,
            trusted_proxies: state.cfg.security.trusted_proxies.clone(),
        };
        // Pre-warm the in-memory post cache so the first /blog hit
        // doesn't pay the parse cost.
        let _ = marketing::blog::init();
        // Layered here, not inside marketing::router, to keep the marketing
        // module free of `crate::observability` imports — its hard-isolation
        // contract (see marketing/mod.rs) limits which crate paths it may
        // depend on, and a future service extraction stays a clean cut.
        // Assistant crawlers only ever fetch this surface, and they run no
        // JavaScript, so the browser tracker cannot see them.
        let marketing_router = marketing::router(marketing_cfg)
            .layer(middleware::from_fn(observability::ai_traffic::middleware))
            .layer(middleware::from_fn(request::http_metrics::middleware));
        let dispatch = marketing::RouteByHost {
            scheme,
            marketing: marketing_router,
            app: app_router,
        };
        Router::new().fallback_service(dispatch)
    } else {
        app_router
    };

    let router = combined.layer(middleware::from_fn(access_log)).layer(
        TraceLayer::new_for_http()
            .make_span_with(|req: &axum::extract::Request| {
                let path = req.uri().path();
                // Caddy active-health and the deploy gate poll these
                // on a tight loop forever; a span per probe at full
                // sampling is pure noise with no diagnostic value.
                if is_health_path(path) {
                    return tracing::Span::none();
                }
                // Path only, never the query string: /auth/* carries
                // single-use magic-link tokens and the OAuth
                // code/state, which must not reach stdout logs or the
                // exported span. The /m/ share token is a path segment,
                // scrubbed here so it never lands in an exported span.
                let path = scrub_capability_token(path);
                tracing::debug_span!(
                    "http.request",
                    method = %req.method(),
                    path = %path,
                )
            })
            .on_response(DefaultOnResponse::new().level(tracing::Level::DEBUG)),
    );

    let listener = TcpListener::bind(&api_bind).await.map_err(AppError::Io)?;
    tracing::info!(
        // SAFE: operator API bind address, not a peer/user IP
        addr = %api_bind,
        "api listening"
    );

    let signal_token = root.clone();
    let serve = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        wait_for_signal().await;
        signal_token.cancel();
    });

    if let Err(err) = serve.await {
        tracing::error!(?err, "api server error");
        root.cancel();
    }

    tracing::info!("draining background tasks");
    let drain = async {
        let _ = tokio::join!(
            scheduler_handle,
            batcher_handle,
            org_ttl_handle,
            sampler_handle,
            incident_writer_handle,
            agent_health_handle,
            inventory_handle,
            channel_health_handle,
            region_health_handle,
            error_class_handle,
            silence_sweep_handle,
            escalation_engine_handle,
            purge_handle,
            invitation_purge_handle,
            oauth_state_cleanup_handle,
            webauthn_state_cleanup_handle,
            channel_verification_cleanup_handle,
            subscriber_dispatch_handle,
            subscriber_token_cleanup_handle,
            subscriber_delivery_cleanup_handle,
            heartbeat_prev_token_cleanup_handle,
            heartbeat_nudge_handle,
            plan_holds_handle,
        );
        if let Some(h) = magic_link_cleanup_handle {
            let _ = h.await;
        }
        if let Some(h) = billing_sweep_handle {
            let _ = h.await;
        }
        if let Some(billing) = billing_for_drain {
            billing.drain().await;
        }
        if let Some(h) = abuse_reload_handle {
            let _ = h.await;
        }
        if let Some(h) = disposable_refresh_handle {
            let _ = h.await;
        }
        if let Some(h) = snitch_handle {
            let _ = h.await;
        }
        if let Some(h) = local_dispatch_handle {
            let _ = h.await;
        }
    };
    if timeout(SHUTDOWN_DEADLINE, drain).await.is_err() {
        tracing::warn!(
            deadline_secs = SHUTDOWN_DEADLINE.as_secs(),
            "shutdown deadline exceeded, dropping in-flight work"
        );
    }

    let _ = metrics_handle;
    // Flush and stop the OTLP batch exporter last, after the subscriber
    // has captured the final shutdown spans/logs.
    tracing_guard.shutdown();
    tracing::info!("shutdown complete");
    Ok(())
}

async fn wait_for_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install ctrl-c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT"),
        _ = terminate => tracing::info!("received SIGTERM"),
    }
}

#[cfg(test)]
mod tests {
    use super::scrub_capability_token;

    #[test]
    fn scrub_capability_token_masks_the_capability_segment() {
        // The token is a path-segment secret; the placeholder must not echo it.
        assert_eq!(scrub_capability_token("/m/abc123secret"), "/m/{token}");
        assert_eq!(
            scrub_capability_token("/m/abc123secret/live"),
            "/m/{token}/live"
        );
        assert_eq!(
            scrub_capability_token("/m/abc123secret/incidents"),
            "/m/{token}/incidents"
        );
        assert_eq!(
            scrub_capability_token("/m/abc/latency"),
            "/m/{token}/latency"
        );
    }

    #[test]
    fn scrub_capability_token_leaves_other_paths_untouched() {
        assert_eq!(scrub_capability_token("/targets/123"), "/targets/123");
        assert_eq!(scrub_capability_token("/"), "/");
        // A path that merely starts with /m but isn't the share surface.
        assert_eq!(scrub_capability_token("/metrics"), "/metrics");
    }
}
