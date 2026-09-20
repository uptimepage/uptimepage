//! Stateless regional probe. Pulls its region's monitor config from the
//! control plane, runs the probe executors, ships results back. Runs no local
//! Postgres/ClickHouse/web/alerting — config lives in memory and a restart
//! re-pulls.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use backoff::ExponentialBackoffBuilder;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper::header::{self, AUTHORIZATION, IF_NONE_MATCH};
use hyper::{Request, StatusCode};
use metrics::counter;
use secrecy::ExposeSecret;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::config::AppConfig;
use crate::domain::agent_wire::{
    AgentTargetsResponse, DispatchBatch, DispatchReport, DispatchedCheck, FlowRunRecord,
    IngestRequestRef,
};
use crate::domain::{CheckResult, OrgId, Target};
use crate::error::{AppError, Result};
use crate::http_client::HttpClients;
use crate::http_client::client::build_clients;
use crate::http_outbound::{self, OutboundHttpClient};
use crate::metric_names;
use crate::pipeline::{BatcherConfig, ResultBatcher};
use crate::scheduler::{Scheduler, TargetRegistry};
use crate::security::SsrfGuard;
use crate::storage::admin::EnabledTargetSource;
use crate::storage::traits::FlowRunSink;
use crate::storage::{DomainExpiryStateStore, InMemoryDomainExpiryStateStore, ResultSink};
use crate::worker::domain_expiry::{DEFAULT_MAX_STALENESS, DomainExpiryRuntime};
use crate::worker::host_throttle::HostThrottle;
use crate::worker::rdap::RdapClient;
use crate::worker::rdap_singleflight::RdapSingleflight;
use crate::worker::{ResultFanout, WorkerDeps, WorkerPool};

const MAX_PULL_BYTES: usize = 32 << 20;
/// One exchange with the control plane, headers and body together. A stall
/// mid-body wedges this loop, and the region then stops probing while still
/// reporting itself connected.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

struct PullCache {
    etag: Option<String>,
    targets: Vec<(OrgId, Target)>,
}

/// [`EnabledTargetSource`] that pulls from the control plane instead of
/// Postgres. Caches the last response so a `304` or a transient control-plane
/// outage keeps the agent running on last-known config.
pub struct AgentPullSource {
    client: OutboundHttpClient,
    url: String,
    token: String,
    flow_capable: bool,
    cache: Mutex<PullCache>,
}

impl AgentPullSource {
    pub fn new(
        client: OutboundHttpClient,
        base_url: &str,
        token: String,
        flow_capable: bool,
    ) -> Self {
        Self {
            client,
            url: format!("{}/api/agent/targets", base_url.trim_end_matches('/')),
            token,
            flow_capable,
            cache: Mutex::new(PullCache {
                etag: None,
                targets: Vec::new(),
            }),
        }
    }

    async fn cached_or_err(&self, msg: String) -> Result<Vec<(OrgId, Target)>> {
        let cache = self.cache.lock().await;
        if cache.etag.is_some() || !cache.targets.is_empty() {
            tracing::warn!(error = %msg, "agent config pull failed; serving last-known config");
            Ok(cache.targets.clone())
        } else {
            Err(AppError::Other(anyhow::anyhow!(msg)))
        }
    }
}

#[async_trait]
impl EnabledTargetSource for AgentPullSource {
    async fn list_all_enabled_targets(&self) -> Result<Vec<(OrgId, Target)>> {
        let etag = self.cache.lock().await.etag.clone();
        let mut builder =
            Request::get(&self.url).header(AUTHORIZATION, format!("Bearer {}", self.token));
        if self.flow_capable {
            builder = builder.header("x-flow-capable", "true");
        }
        if let Some(e) = &etag {
            builder = builder.header(IF_NONE_MATCH, e.clone());
        }
        let req = builder
            .body(Full::new(Bytes::new()))
            .context("building config-pull request")?;

        let at = tokio::time::Instant::now() + HTTP_TIMEOUT;
        let resp = match tokio::time::timeout_at(at, self.client.request(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                return self
                    .cached_or_err(format!("config pull request failed: {e}"))
                    .await;
            }
            Err(_) => {
                return self
                    .cached_or_err("config pull request timed out".into())
                    .await;
            }
        };

        let status = resp.status();
        if status == StatusCode::NOT_MODIFIED {
            return Ok(self.cache.lock().await.targets.clone());
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            // Terminal: the token was revoked or the agent disabled. Drop the
            // cached config and schedule nothing — serving last-known here would
            // keep probing (and holding decrypted credentials) after an operator
            // pulled the plug. Resumes automatically if the credential is restored.
            let mut cache = self.cache.lock().await;
            cache.etag = None;
            cache.targets.clear();
            tracing::error!(%status, "agent credential rejected; pausing until restored");
            return Ok(Vec::new());
        }
        if !status.is_success() {
            // Transient (5xx, connect, timeout): keep running on last-known config.
            return self
                .cached_or_err(format!("control plane returned {status} on config pull"))
                .await;
        }

        let new_etag = resp
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = match tokio::time::timeout_at(
            at,
            Limited::new(resp.into_body(), MAX_PULL_BYTES).collect(),
        )
        .await
        {
            Ok(r) => {
                r.map_err(|e| AppError::Other(anyhow::anyhow!("reading config-pull body: {e}")))?
            }
            Err(_) => {
                return self
                    .cached_or_err("config pull body timed out".into())
                    .await;
            }
        }
        .to_bytes();
        let parsed: AgentTargetsResponse =
            serde_json::from_slice(&bytes).context("decoding config-pull response")?;
        let targets: Vec<(OrgId, Target)> = parsed
            .targets
            .into_iter()
            .map(|d| (OrgId(d.org_id), d.target))
            .collect();

        let mut cache = self.cache.lock().await;
        cache.etag = new_etag;
        cache.targets = targets.clone();
        Ok(targets)
    }
}

/// Flow runs waiting for the next result batch to carry them home. Bounded, so
/// a control plane that stays unreachable costs the oldest runs rather than the
/// agent's memory.
const MAX_PENDING_FLOW_RUNS: usize = 512;

/// Holds flow runs between the worker that produced them and the result batch
/// that ships them. They ride along rather than taking their own request: the
/// ingest endpoint already authenticates the region, dedups the batch, and
/// checks the target assignment they need.
#[derive(Default)]
pub struct PendingFlowRuns {
    runs: std::sync::Mutex<std::collections::VecDeque<FlowRunRecord>>,
}

impl PendingFlowRuns {
    fn take(&self) -> Vec<FlowRunRecord> {
        let mut q = self.runs.lock().expect("pending flow runs poisoned");
        q.drain(..).collect()
    }

    /// Put a drained batch back at the front when its request never landed, so
    /// a control-plane blip costs a delay rather than the history.
    fn restore(&self, runs: Vec<FlowRunRecord>) {
        let mut q = self.runs.lock().expect("pending flow runs poisoned");
        for run in runs.into_iter().rev() {
            if q.len() >= MAX_PENDING_FLOW_RUNS {
                q.pop_back();
                counter!(metric_names::STORAGE_DROPPED, "reason" => "flow_run_buffer_full")
                    .increment(1);
            }
            q.push_front(run);
        }
    }
}

#[async_trait]
impl FlowRunSink for PendingFlowRuns {
    async fn write_runs(&self, runs: &[FlowRunRecord]) {
        let mut q = self.runs.lock().expect("pending flow runs poisoned");
        for run in runs {
            if q.len() >= MAX_PENDING_FLOW_RUNS {
                q.pop_front();
                counter!(metric_names::STORAGE_DROPPED, "reason" => "flow_run_buffer_full")
                    .increment(1);
            }
            q.push_back(run.clone());
        }
    }
}

/// [`ResultSink`] that POSTs batches to the control-plane ingest API. Region
/// and agent id are not sent — the control plane derives them from the bearer
/// token, so they can't be spoofed. A fresh `batch_id` per call, reused across
/// the internal retries, makes a lost ack idempotent.
pub struct AgentResultSink {
    client: OutboundHttpClient,
    url: Url,
    token: String,
    /// Drained into the batch below, so a flow run reaches the control plane on
    /// the request that carries the verdict it belongs to.
    flow_runs: Arc<PendingFlowRuns>,
}

#[async_trait]
impl ResultSink for AgentResultSink {
    async fn write_batch(&self, results: &[CheckResult]) -> Result<()> {
        if results.is_empty() {
            return Ok(());
        }
        // Drained once, outside the retry, so every attempt re-sends the same
        // body and the batch stays deduplicable by its id.
        let flow_runs = self.flow_runs.take();
        // One id for this batch, reused across the internal retries below so a
        // lost ack (POST landed, response dropped) re-sends the SAME id and the
        // dashboard dedups it instead of double-writing. Retrying here — not
        // letting the batcher re-flush — is what keeps the id stable: the
        // batcher reforms (supersets) the buffer between ticks.
        let batch_id = Uuid::new_v4();
        let mut headers = std::collections::BTreeMap::new();
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", self.token),
        );
        let backoff = ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_millis(200))
            .with_max_interval(Duration::from_secs(5))
            .with_max_elapsed_time(Some(Duration::from_secs(30)))
            .build();
        let op = || async {
            let body = IngestRequestRef {
                batch_id,
                results,
                flow_runs: &flow_runs,
            };
            http_outbound::post_json_with_headers(&self.client, &self.url, &body, &headers)
                .await
                .map_err(backoff::Error::transient)
        };
        let sent = backoff::future::retry(backoff, op).await;
        if sent.is_err() {
            self.flow_runs.restore(flow_runs);
        }
        sent
    }
}

/// Long-polls the control plane for interactive check checks (test / check-now),
/// runs each with the same executor as scheduled checks, and posts the result
/// back. The claim request is held open by the brain until a check arrives (or
/// the hold window elapses), so a check runs within ~ms of a button press with no
/// busy polling. Kept separate from the scheduled-result pipeline: results
/// carry a probe preview and are correlated by `check_id`, not batched.
struct AgentDispatchClient {
    client: OutboundHttpClient,
    claim_url: String,
    results_url: Url,
    token: String,
    http_clients: Arc<HttpClients>,
    domain_runtime: Arc<DomainExpiryRuntime>,
    flow_engine: Option<Arc<crate::worker::flow::engine::CdpEngine>>,
}

/// Backoff before reconnecting a long-poll that errored (control plane down).
const DISPATCH_RECONNECT_BACKOFF: Duration = Duration::from_secs(3);

impl AgentDispatchClient {
    async fn run(self, cancel: CancellationToken) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                r = self.poll_once() => {
                    if let Err(err) = r {
                        tracing::warn!(error = %err, "agent dispatch long-poll failed; backing off");
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(DISPATCH_RECONNECT_BACKOFF) => {}
                        }
                    }
                }
            }
        }
    }

    async fn poll_once(&self) -> Result<()> {
        for check in self.claim().await? {
            let deps = WorkerDeps {
                http: self.http_clients.as_ref(),
                domain_expiry: self.domain_runtime.as_ref(),
                flow: self.flow_engine.as_deref(),
            };
            let target_id = check.target_id.unwrap_or_else(Uuid::nil);
            let (result, probe) =
                crate::worker::execute_with_probe(target_id, check.org_id, &check.spec, &deps)
                    .await;
            let payload = DispatchReport {
                check_id: check.id,
                result,
                response_headers_preview: probe.response_headers_preview,
                response_body_snippet: probe.response_body_snippet,
                flow_evidence: probe.flow_evidence,
                flow_steps: probe.flow_steps,
            };
            self.post_result(&payload).await?;
        }
        Ok(())
    }

    async fn claim(&self) -> Result<Vec<DispatchedCheck>> {
        let req = Request::get(&self.claim_url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .body(Full::new(Bytes::new()))
            .context("building dispatch-claim request")?;
        let at = tokio::time::Instant::now() + HTTP_TIMEOUT;
        let resp = match tokio::time::timeout_at(at, self.client.request(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                return Err(AppError::Other(anyhow::anyhow!(
                    "dispatch claim failed: {e}"
                )));
            }
            Err(_) => return Err(AppError::Other(anyhow::anyhow!("dispatch claim timed out"))),
        };
        let status = resp.status();
        // Terminal credential rejection is handled (and logged) by the config
        // pull loop; here just back off quietly.
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Ok(Vec::new());
        }
        if !status.is_success() {
            return Err(AppError::Other(anyhow::anyhow!(
                "control plane returned {status} on dispatch claim"
            )));
        }
        let bytes =
            tokio::time::timeout_at(at, Limited::new(resp.into_body(), MAX_PULL_BYTES).collect())
                .await
                .map_err(|_| AppError::Other(anyhow::anyhow!("dispatch claim body timed out")))?
                .map_err(|e| AppError::Other(anyhow::anyhow!("reading dispatch-claim body: {e}")))?
                .to_bytes();
        let parsed: DispatchBatch =
            serde_json::from_slice(&bytes).context("decoding dispatch-claim response")?;
        Ok(parsed.checks)
    }

    async fn post_result(&self, payload: &DispatchReport) -> Result<()> {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", self.token),
        );
        http_outbound::post_json_with_headers(&self.client, &self.results_url, payload, &headers)
            .await
    }
}

/// Run the process as a regional agent until SIGINT/SIGTERM.
pub async fn run(cfg: AppConfig) -> Result<()> {
    let region = cfg.agent.region.clone();
    let base = cfg
        .agent
        .control_plane_url
        .trim_end_matches('/')
        .to_string();
    let token = cfg.agent.token.expose_secret().to_string();
    tracing::info!(
        region = %region,
        control_plane = %base,
        "starting regional agent"
    );

    // The control-plane URL is operator-supplied (trusted), not attacker
    // input, so it honors the same `allow_private_targets` knob as the probe
    // client. Default (knob off) keeps the strict guard; a local/integration
    // control plane on a private address opts in via the flag.
    let outbound =
        http_outbound::build_outbound_client(SsrfGuard::new(cfg.security.allow_private_targets));

    let flow_capable = cfg.flow.enabled;
    let source: Arc<dyn EnabledTargetSource> = Arc::new(AgentPullSource::new(
        outbound.clone(),
        &base,
        token.clone(),
        flow_capable,
    ));
    let results_url = Url::parse(&format!("{base}/api/agent/results"))
        .context("agent.control_plane_url is not a valid URL")?;
    let pending_flow_runs = Arc::new(PendingFlowRuns::default());
    let sink: Arc<dyn ResultSink> = Arc::new(AgentResultSink {
        client: outbound,
        url: results_url,
        token: token.clone(),
        flow_runs: pending_flow_runs.clone(),
    });

    let http_clients = Arc::new(build_clients(
        &cfg.http_client,
        &cfg.checker,
        &cfg.dns,
        &cfg.security,
    )?);
    let (result_tx, result_rx) = tokio::sync::mpsc::channel(cfg.agent.buffer_capacity.max(1024));
    let fanout = ResultFanout::new(result_tx.clone());
    let host_throttle = Arc::new(HostThrottle::new(
        cfg.checker.per_host_max_inflight,
        cfg.checker.rdap_max_inflight,
    ));
    let rdap_client = Arc::new(RdapClient::new(http_outbound::build_outbound_client(
        SsrfGuard::strict(),
    )));
    let domain_state: Arc<dyn DomainExpiryStateStore> =
        Arc::new(InMemoryDomainExpiryStateStore::new());
    let domain_runtime = Arc::new(DomainExpiryRuntime::new(
        Arc::new(crate::worker::registration::RegistrationClient::new(
            rdap_client,
        )),
        Arc::new(RdapSingleflight::with_default_ttl()),
        domain_state,
        host_throttle.clone(),
        DEFAULT_MAX_STALENESS,
    ));
    let flow_engine = flow_capable.then(|| {
        Arc::new(crate::worker::flow::engine::CdpEngine::new(
            crate::worker::flow::engine::FlowEngineConfig {
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
            host_throttle,
            domain_runtime.clone(),
        )
        .with_flow_engine(flow_engine.clone())
        .with_flow_runs(flow_capable.then_some(pending_flow_runs as Arc<dyn FlowRunSink>)),
    );
    let registry = Arc::new(TargetRegistry::new(source));
    let mut sched_cfg = cfg.scheduler.clone();
    sched_cfg.target_refresh_interval_secs = cfg.agent.pull_interval_secs;
    let scheduler = Arc::new(Scheduler::new(registry.clone(), pool.clone(), sched_cfg));
    let batcher = ResultBatcher::new(
        result_rx,
        sink,
        BatcherConfig {
            batch_size: cfg.storage.clickhouse.batch_size.max(1),
            batch_timeout: Duration::from_secs(cfg.agent.flush_interval_secs.max(1)),
        },
    );

    let root = CancellationToken::new();
    let scheduler_handle = {
        let scheduler = scheduler.clone();
        let token = root.clone();
        tokio::spawn(async move {
            if let Err(err) = scheduler.run(token).await {
                tracing::error!(?err, "agent scheduler exited with error");
            }
        })
    };
    let batcher_handle = {
        let token = root.clone();
        tokio::spawn(async move { batcher.run(token).await })
    };
    // Probe-side gauges (workers in-flight, open breakers, queue depth, RSS).
    // No DbSources: the agent holds no Postgres/ClickHouse pool to sample.
    let sample_interval =
        Duration::from_millis(cfg.observability.gauge_sample_interval_ms.max(100));
    let sampler_handle = crate::scheduler::sampler::spawn(
        pool,
        registry,
        None,
        &result_tx,
        sample_interval,
        root.clone(),
    );
    drop(result_tx);
    let job_client = AgentDispatchClient {
        client: http_outbound::build_outbound_client(SsrfGuard::new(
            cfg.security.allow_private_targets,
        )),
        claim_url: format!("{base}/api/agent/dispatch"),
        results_url: Url::parse(&format!("{base}/api/agent/dispatch/results"))
            .context("agent.control_plane_url is not a valid URL")?,
        token,
        http_clients: http_clients.clone(),
        domain_runtime,
        flow_engine,
    };
    let job_handle = {
        let token = root.clone();
        tokio::spawn(async move { job_client.run(token).await })
    };

    wait_for_signal().await;
    tracing::info!("agent shutting down");
    root.cancel();
    let _ = scheduler_handle.await;
    let _ = batcher_handle.await;
    let _ = sampler_handle.await;
    let _ = job_handle.await;
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return,
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
