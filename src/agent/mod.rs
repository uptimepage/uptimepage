//! Stateless regional probe. Pulls its region's monitor config from the
//! control plane, runs the probe executors, ships results back. Runs no local
//! Postgres/ClickHouse/web/alerting — config lives in memory and a restart
//! re-pulls.

use std::convert::Infallible;
use std::io::Read as _;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use backoff::ExponentialBackoffBuilder;
use flate2::read::GzDecoder;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Body, Bytes};
use hyper::header::{self, ACCEPT_ENCODING, AUTHORIZATION, HeaderName, HeaderValue, IF_NONE_MATCH};
use hyper::{HeaderMap, Request, StatusCode};
use metrics::counter;
use secrecy::ExposeSecret;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::config::AppConfig;
use crate::domain::agent_wire::{
    AGENT_BUILD, AGENT_MONITORS, AgentTargetsResponse, DispatchBatch, DispatchReport,
    DispatchedCheck, FlowRunRecord, IngestRequestRef, MAX_FLOW_RUNS_PER_BATCH, reported_build,
};
use crate::domain::{CheckResult, MAX_CHECK_TIMEOUT, OrgId, Target};
use crate::error::{AppError, Result};
use crate::http_client::HttpClients;
use crate::http_client::client::build_clients;
use crate::http_outbound::{self, OutboundHttpClient};
use crate::metric_names;
use crate::pipeline::{BatcherConfig, ResultBatcher};
use crate::scheduler::{Scheduler, TargetRegistry, stop_probing};
use crate::security::SsrfGuard;
use crate::storage::admin::{EnabledTargetSource, NothingToProbe};
use crate::storage::traits::FlowRunSink;
use crate::storage::{DomainExpiryStateStore, InMemoryDomainExpiryStateStore, ResultSink};
use crate::worker::domain_expiry::{DEFAULT_MAX_STALENESS, DomainExpiryRuntime};
use crate::worker::host_throttle::HostThrottle;
use crate::worker::rdap::RdapClient;
use crate::worker::rdap_singleflight::RdapSingleflight;
use crate::worker::{ResultFanout, WorkerDeps, WorkerPool};

/// Past the longest check budget, since a check can overrun it slightly, and
/// inside the 180 s stop grace period with the final flush's own retries.
const SETTLE: Duration = Duration::from_secs(MAX_CHECK_TIMEOUT.as_secs() + 15);

pub const MAX_PULL_BYTES: usize = 32 << 20;
/// The headers of a config pull, or a whole dispatch claim.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// A stall mid-body wedges the pull loop, and the region then stops following
/// its config while still reporting itself connected.
const PULL_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// Bounds a body that trickles without ever stalling.
const PULL_BODY_TIMEOUT: Duration = Duration::from_secs(300);
/// The control plane records an agent visit at most every 30 s and, by
/// default, calls the agent stale after 90 s, so even with one pull lost this
/// one stays live.
const SEEN_EVERY: Duration = Duration::from_secs(20);
const SEEN_TIMEOUT: Duration = Duration::from_secs(5);
/// Longer than a dead connection takes to notice, so a post caught on one can
/// still go out on a new one.
const POST_RETRY_BUDGET: Duration = Duration::from_secs(30);

/// How the control plane answered a pull whose body nobody reads.
enum Touched {
    /// With the version it named, if any.
    Answered(Option<String>),
    Rejected(StatusCode),
}

fn rejects_credential(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN
}

/// A config the control plane served. An empty one with no `ETag` is still
/// loaded: the region has nothing to probe, which is not the same as not knowing.
struct LoadedConfig {
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
    build: Option<HeaderValue>,
    cache: Mutex<Option<LoadedConfig>>,
}

impl AgentPullSource {
    /// `build` goes out on every config pull, unless it is not a value the
    /// control plane would record, such as the empty commit of a build with
    /// no git context.
    pub fn new(
        client: OutboundHttpClient,
        base_url: &str,
        token: String,
        flow_capable: bool,
        build: &str,
    ) -> Self {
        Self {
            client,
            url: format!("{}/api/agent/targets", base_url.trim_end_matches('/')),
            token,
            flow_capable,
            build: reported_build(build).and_then(|b| HeaderValue::from_str(b).ok()),
            cache: Mutex::new(None),
        }
    }

    /// The version a pull asks about and the monitor count it reports, both
    /// `None` while no config is loaded.
    async fn held(&self) -> (Option<String>, Option<usize>) {
        let cache = self.cache.lock().await;
        cache
            .as_ref()
            .map_or((None, None), |c| (c.etag.clone(), Some(c.targets.len())))
    }

    /// Terminal: the token was revoked or the agent disabled. Serving
    /// last-known here would keep probing, and holding decrypted credentials,
    /// after an operator pulled the plug. Resumes once the credential is restored.
    async fn unload(&self, status: StatusCode) {
        *self.cache.lock().await = None;
        tracing::error!(%status, "agent credential rejected; pausing until restored");
    }

    async fn cached_or_err(&self, msg: String) -> Result<Vec<(OrgId, Target)>> {
        match self.cache.lock().await.as_ref() {
            Some(held) => {
                tracing::warn!(error = %msg, "agent config pull failed; serving last-known config");
                Ok(held.targets.clone())
            }
            None => Err(AppError::Other(NothingToProbe(msg).into())),
        }
    }

    fn pull_request(
        &self,
        etag: Option<&str>,
        monitors: Option<usize>,
    ) -> Result<Request<Full<Bytes>>> {
        let held = monitors.map_or(HeaderValue::from_static("none"), HeaderValue::from);
        let mut builder = Request::get(&self.url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header(ACCEPT_ENCODING, "gzip")
            .header(AGENT_MONITORS, held);
        if self.flow_capable {
            builder = builder.header("x-flow-capable", "true");
        }
        if let Some(build) = &self.build {
            builder = builder.header(AGENT_BUILD, build.clone());
        }
        if let Some(e) = etag {
            builder = builder.header(IF_NONE_MATCH, e);
        }
        Ok(builder
            .body(Full::new(Bytes::new()))
            .context("building config-pull request")?)
    }

    /// Runs `stopping` while pulling every `every`, only so the control plane
    /// keeps counting this agent live while its last checks settle. Replies
    /// are dropped unread: nothing schedules from them any more.
    pub async fn keep_seen_while(&self, stopping: impl Future<Output = ()>, every: Duration) {
        tokio::select! {
            () = stopping => {}
            never = self.keep_seen(every) => match never {},
        }
    }

    /// Tracks the config version on its own, so a change made while settling
    /// is sent in full once rather than on every pull, and the cache stays as
    /// the scheduler last read it. The monitor count still comes from the
    /// cache: a version adopted from a reply nobody read loads nothing. The
    /// first pull waits a full period: the scheduler pulled moments ago.
    async fn keep_seen(&self, every: Duration) -> Infallible {
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        ticks.tick().await;
        let (mut etag, _) = self.held().await;
        loop {
            let (_, monitors) = self.held().await;
            match self.touch(etag.as_deref(), monitors).await {
                Ok(Touched::Answered(answered)) => etag = answered.or(etag),
                Ok(Touched::Rejected(status)) => {
                    tracing::warn!(%status, "agent keep-alive pull rejected");
                }
                Err(err) => tracing::warn!(error = %err, "agent keep-alive pull failed"),
            }
            ticks.tick().await;
        }
    }

    async fn touch(&self, etag: Option<&str>, monitors: Option<usize>) -> Result<Touched> {
        let req = self.pull_request(etag, monitors)?;
        let resp = tokio::time::timeout(SEEN_TIMEOUT, self.client.request(req))
            .await
            .context("timed out")?
            .map_err(|e| {
                let cause = http_outbound::error_chain(e);
                AppError::Other(anyhow::anyhow!("request failed: {cause}"))
            })?;
        let status = resp.status();
        if rejects_credential(status) {
            return Ok(Touched::Rejected(status));
        }
        if status != StatusCode::NOT_MODIFIED && !status.is_success() {
            return Err(AppError::Other(anyhow::anyhow!(
                "control plane returned {status}"
            )));
        }
        Ok(Touched::Answered(text_header(resp.headers(), header::ETAG)))
    }
}

fn text_header(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

#[derive(Debug)]
enum PullBodyError {
    OverLimit,
    Failed(String),
}

/// Reads `body` for as long as it keeps arriving.
async fn collect_moving<B>(body: B) -> std::result::Result<Vec<u8>, PullBodyError>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut body = Limited::new(body, MAX_PULL_BYTES);
    let mut collected = Vec::new();
    let read = async {
        loop {
            let frame = tokio::time::timeout(PULL_IDLE_TIMEOUT, body.frame())
                .await
                .map_err(|_| PullBodyError::Failed("config pull body stalled".into()))?;
            match frame {
                None => return Ok(()),
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        collected.extend_from_slice(data);
                    }
                }
                Some(Err(e)) if e.is::<LengthLimitError>() => {
                    return Err(PullBodyError::OverLimit);
                }
                Some(Err(e)) => {
                    let cause = http_outbound::error_chain(e);
                    return Err(PullBodyError::Failed(format!(
                        "reading config-pull body: {cause}"
                    )));
                }
            }
        }
    };
    tokio::time::timeout(PULL_BODY_TIMEOUT, read)
        .await
        .map_err(|_| PullBodyError::Failed("config pull body timed out".into()))??;
    Ok(collected)
}

fn decoded(raw: Vec<u8>, encoding: Option<&str>) -> std::result::Result<Vec<u8>, PullBodyError> {
    let Some(encoding) = encoding else {
        return Ok(raw);
    };
    if !encoding.eq_ignore_ascii_case("gzip") {
        return Err(PullBodyError::Failed(format!(
            "config pull body came encoded as {encoding}"
        )));
    }
    let mut out = Vec::new();
    GzDecoder::new(raw.as_slice())
        .take(MAX_PULL_BYTES as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| PullBodyError::Failed(format!("decompressing config-pull body: {e}")))?;
    if out.len() > MAX_PULL_BYTES {
        return Err(PullBodyError::OverLimit);
    }
    Ok(out)
}

#[async_trait]
impl EnabledTargetSource for AgentPullSource {
    async fn list_all_enabled_targets(&self) -> Result<Vec<(OrgId, Target)>> {
        let (etag, monitors) = self.held().await;
        let req = self.pull_request(etag.as_deref(), monitors)?;

        let resp = match tokio::time::timeout(HTTP_TIMEOUT, self.client.request(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                let cause = http_outbound::error_chain(e);
                return self
                    .cached_or_err(format!("config pull request failed: {cause}"))
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
            let cache = self.cache.lock().await;
            return Ok(cache.as_ref().map_or_else(Vec::new, |c| c.targets.clone()));
        }
        if rejects_credential(status) {
            self.unload(status).await;
            return Ok(Vec::new());
        }
        if !status.is_success() {
            // Transient (5xx, connect, timeout): keep running on last-known config.
            return self
                .cached_or_err(format!("control plane returned {status} on config pull"))
                .await;
        }

        let new_etag = text_header(resp.headers(), header::ETAG);
        let encoding = text_header(resp.headers(), header::CONTENT_ENCODING);
        let body = collect_moving(resp.into_body())
            .await
            .and_then(|raw| decoded(raw, encoding.as_deref()));
        let bytes = match body {
            Ok(bytes) => bytes,
            Err(PullBodyError::OverLimit) => {
                return Err(AppError::Other(anyhow::anyhow!(
                    "config pull body is over {MAX_PULL_BYTES} bytes"
                )));
            }
            Err(PullBodyError::Failed(msg)) => return self.cached_or_err(msg).await,
        };
        let parsed: AgentTargetsResponse =
            serde_json::from_slice(&bytes).context("decoding config-pull response")?;
        let targets: Vec<(OrgId, Target)> = parsed
            .targets
            .into_iter()
            .map(|d| (OrgId(d.org_id), d.target))
            .collect();

        let first_load = self
            .cache
            .lock()
            .await
            .replace(LoadedConfig {
                etag: new_etag.clone(),
                targets: targets.clone(),
            })
            .is_none();
        if first_load {
            // Tells the control plane at once rather than a pull interval later.
            match self.touch(new_etag.as_deref(), Some(targets.len())).await {
                Ok(Touched::Answered(_)) => {}
                Ok(Touched::Rejected(status)) => {
                    self.unload(status).await;
                    return Ok(Vec::new());
                }
                Err(err) => tracing::warn!(error = %err, "agent config load confirm failed"),
            }
        }
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
    /// At most what ingest accepts: it refuses a larger batch outright, so a
    /// backlog sent whole would fail every batch until the agent restarts.
    fn take(&self) -> Vec<FlowRunRecord> {
        let mut q = self.runs.lock().expect("pending flow runs poisoned");
        let batch = q.len().min(MAX_FLOW_RUNS_PER_BATCH);
        q.drain(..batch).collect()
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
        // Reused across the retries below, so a lost ack (POST landed, reply
        // dropped) resends the same id and the control plane skips it. Once
        // they give up, the batcher resends the same results under a new id,
        // and ClickHouse block dedup drops the copy that already landed.
        let batch_id = Uuid::new_v4();
        let body = IngestRequestRef {
            batch_id,
            results,
            flow_runs: &flow_runs,
        };
        let sent = post_with_retry(&self.client, &self.url, &self.token, &body).await;
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
    pool: Arc<WorkerPool>,
}

/// Backoff before reconnecting a long-poll that errored (control plane down).
const DISPATCH_RECONNECT_BACKOFF: Duration = Duration::from_secs(3);

impl AgentDispatchClient {
    async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let claim = |limit| self.claim_or_back_off(limit);
        let run = |check| {
            let this = self.clone();
            async move { this.execute(check).await }
        };
        crate::ad_hoc_dispatch::serve_claims(claim, run, cancel).await;
    }

    async fn claim_or_back_off(&self, limit: usize) -> Vec<DispatchedCheck> {
        match self.claim(limit).await {
            Ok(checks) => checks,
            Err(err) => {
                tracing::warn!(error = %err, "agent dispatch long-poll failed; backing off");
                tokio::time::sleep(DISPATCH_RECONNECT_BACKOFF).await;
                Vec::new()
            }
        }
    }

    async fn execute(&self, check: DispatchedCheck) {
        let deps = WorkerDeps {
            http: self.http_clients.as_ref(),
            domain_expiry: self.domain_runtime.as_ref(),
            flow: self.flow_engine.as_deref(),
        };
        let target_id = check.target_id.unwrap_or_else(Uuid::nil);
        let (result, probe) =
            crate::worker::execute_with_probe(target_id, check.org_id, &check.spec, &deps).await;
        if let Some(id) = check.target_id {
            self.pool.note_check_now(id, &result);
        }
        let payload = DispatchReport {
            check_id: check.id,
            result,
            response_headers_preview: probe.response_headers_preview,
            response_body_snippet: probe.response_body_snippet,
            flow_evidence: probe.flow_evidence,
            flow_steps: probe.flow_steps,
        };
        let posted = post_with_retry(&self.client, &self.results_url, &self.token, &payload).await;
        if let Err(err) = posted {
            tracing::warn!(error = %err, check_id = %check.id, "agent dispatch result post failed");
        }
    }

    async fn claim(&self, limit: usize) -> Result<Vec<DispatchedCheck>> {
        let req = Request::get(format!("{}?limit={limit}", self.claim_url))
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .body(Full::new(Bytes::new()))
            .context("building dispatch-claim request")?;
        let at = tokio::time::Instant::now() + HTTP_TIMEOUT;
        let resp = match tokio::time::timeout_at(at, self.client.request(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                let cause = http_outbound::error_chain(e);
                return Err(AppError::Other(anyhow::anyhow!(
                    "dispatch claim failed: {cause}"
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
                .map_err(|e| {
                    let cause = http_outbound::error_chain(e);
                    AppError::Other(anyhow::anyhow!("reading dispatch-claim body: {cause}"))
                })?
                .to_bytes();
        let parsed: DispatchBatch =
            serde_json::from_slice(&bytes).context("decoding dispatch-claim response")?;
        Ok(parsed.checks)
    }
}

async fn post_with_retry<T: Serialize>(
    client: &OutboundHttpClient,
    url: &Url,
    token: &str,
    body: &T,
) -> Result<()> {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    let backoff = ExponentialBackoffBuilder::new()
        .with_initial_interval(Duration::from_millis(200))
        .with_max_interval(Duration::from_secs(5))
        .with_max_elapsed_time(Some(POST_RETRY_BUDGET))
        .build();
    let op = || async {
        http_outbound::post_json_with_headers(client, url, body, &headers)
            .await
            .map_err(backoff::Error::transient)
    };
    backoff::future::retry(backoff, op).await
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
    let control_plane = http_outbound::build_control_plane_client(SsrfGuard::new(
        cfg.security.allow_private_targets,
    ));

    let flow_capable = cfg.flow.enabled;
    let pull = Arc::new(AgentPullSource::new(
        control_plane.clone(),
        &base,
        token.clone(),
        flow_capable,
        env!("SM_SOURCE_COMMIT"),
    ));
    let source: Arc<dyn EnabledTargetSource> = pull.clone();
    let results_url = Url::parse(&format!("{base}/api/agent/results"))
        .context("agent.control_plane_url is not a valid URL")?;
    let pending_flow_runs = Arc::new(PendingFlowRuns::default());
    let sink: Arc<dyn ResultSink> = Arc::new(AgentResultSink {
        client: control_plane.clone(),
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
    let flow_engine = flow_capable
        .then(|| crate::worker::flow::engine::CdpEngine::new((&cfg.flow).into()).map(Arc::new))
        .transpose()?;
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
    sched_cfg.region = region;
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
    let flush = CancellationToken::new();
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
        let token = flush.clone();
        tokio::spawn(async move { batcher.run(token).await })
    };
    // Probe-side gauges (workers in-flight, open breakers, queue depth, RSS).
    // No Postgres pool: the agent holds no database connection to sample.
    let sample_interval =
        Duration::from_millis(cfg.observability.gauge_sample_interval_ms.max(100));
    let sampler_handle = crate::scheduler::sampler::spawn(
        pool.clone(),
        registry,
        None,
        &result_tx,
        sample_interval,
        root.clone(),
    );
    drop(result_tx);
    let job_client = Arc::new(AgentDispatchClient {
        client: control_plane,
        claim_url: format!("{base}/api/agent/dispatch"),
        results_url: Url::parse(&format!("{base}/api/agent/dispatch/results"))
            .context("agent.control_plane_url is not a valid URL")?,
        token,
        http_clients: http_clients.clone(),
        domain_runtime,
        flow_engine,
        pool: pool.clone(),
    });
    let job_handle = {
        let token = root.clone();
        tokio::spawn(async move { job_client.run(token).await })
    };

    wait_for_signal().await;
    tracing::info!("agent shutting down");
    pull.keep_seen_while(
        stop_probing(
            &root,
            scheduler_handle,
            &pool,
            SETTLE,
            flush,
            batcher_handle,
        ),
        SEEN_EVERY,
    )
    .await;
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

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::task::{Context, Poll};

    use hyper::body::Frame;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;
    use tokio::time::Instant;

    use super::*;

    struct ChannelBody(mpsc::Receiver<Bytes>);

    impl Body for ChannelBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<std::result::Result<Frame<Bytes>, Infallible>>> {
            self.0
                .poll_recv(cx)
                .map(|chunk| chunk.map(|bytes| Ok(Frame::data(bytes))))
        }
    }

    fn dripping(every: Duration, chunks: usize) -> ChannelBody {
        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            for _ in 0..chunks {
                tokio::time::sleep(every).await;
                if tx.send(Bytes::from_static(b" ")).await.is_err() {
                    return;
                }
            }
        });
        ChannelBody(rx)
    }

    fn within_a_second_of(elapsed: Duration, bound: Duration) -> bool {
        elapsed >= bound && elapsed < bound + Duration::from_secs(1)
    }

    #[tokio::test(start_paused = true)]
    async fn a_config_body_that_keeps_arriving_outlasts_the_header_deadline() {
        let chunks = 7;
        let started = Instant::now();

        let body = collect_moving(dripping(PULL_IDLE_TIMEOUT / 2, chunks))
            .await
            .expect("a slow body is not a stalled one");

        assert_eq!(body.len(), chunks);
        assert!(started.elapsed() > HTTP_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_config_body_fails_at_the_idle_bound() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(Bytes::from_static(b"{")).await.unwrap();
        let started = Instant::now();

        let stalled = collect_moving(ChannelBody(rx)).await;

        assert!(matches!(stalled, Err(PullBodyError::Failed(_))));
        assert!(within_a_second_of(started.elapsed(), PULL_IDLE_TIMEOUT));
        drop(tx);
    }

    #[tokio::test(start_paused = true)]
    async fn a_config_body_that_only_trickles_fails_at_the_total_bound() {
        let started = Instant::now();

        let trickled = collect_moving(dripping(PULL_IDLE_TIMEOUT / 2, usize::MAX)).await;

        assert!(matches!(trickled, Err(PullBodyError::Failed(_))));
        assert!(within_a_second_of(started.elapsed(), PULL_BODY_TIMEOUT));
    }

    #[tokio::test]
    async fn a_post_cut_off_by_the_network_is_sent_again() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = attempts.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut scratch = [0u8; 4096];
                let _ = sock.read(&mut scratch).await;
                if seen.fetch_add(1, Ordering::Relaxed) > 0 {
                    let accepted = b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n";
                    let _ = sock.write_all(accepted).await;
                }
            }
        });
        let client = http_outbound::build_control_plane_client(SsrfGuard::relaxed_for_tests());

        post_with_retry(&client, &url, "token", &serde_json::json!({}))
            .await
            .expect("the second attempt lands");

        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn a_flow_run_backlog_goes_out_in_batches_ingest_accepts() {
        let run = FlowRunRecord {
            org_id: Uuid::now_v7(),
            target_id: Uuid::now_v7(),
            timestamp: chrono::Utc::now(),
            status: crate::domain::CheckStatus::Up,
            duration_ms: 1,
            error: None,
            steps: Vec::new(),
            evidence: None,
        };
        let pending = PendingFlowRuns::default();
        pending
            .write_runs(&vec![run; MAX_FLOW_RUNS_PER_BATCH + 1])
            .await;

        assert_eq!(pending.take().len(), MAX_FLOW_RUNS_PER_BATCH);
        assert_eq!(pending.take().len(), 1);
    }
}
