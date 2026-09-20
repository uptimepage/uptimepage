use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::{DashMap, DashSet};
use metrics::{Counter, counter, histogram};
use tokio::sync::{Semaphore, mpsc};

use uuid::Uuid;

use crate::config::CircuitBreakerConfig;
use crate::domain::{CheckResult, CheckSpec, OrgId, Target};
use crate::http_client::HttpClients;
use crate::metric_names;
use crate::worker::circuit_breaker::{BreakerState, CIRCUIT_OPEN_REASON, CircuitBreaker};
use crate::worker::host_throttle::{HostPermit, HostThrottle, Throttled};

// Hot-path counters resolved once. `counter!` rebuilds the label set on
// every call — at high QPS that's a per-check allocation we don't need.
// `host_throttle_waits_total{kind=rdap}` is incremented by the
// domain_expiry executor since RDAP no longer pre-throttles at the pool
// level.
static HOST_THROTTLE_WAITS_HOST: LazyLock<Counter> =
    LazyLock::new(|| counter!(metric_names::HOST_THROTTLE_WAITS, "kind" => "host"));
static HOST_THROTTLE_DROPS_C: LazyLock<Counter> =
    LazyLock::new(|| counter!(metric_names::HOST_THROTTLE_DROPS));

pub struct CheckTask {
    pub target: Arc<Target>,
    /// Owning tenant of `target`. Threaded so a check result fans out to the
    /// alert engine with the org needed for tenant-scoped channel resolution.
    pub org_id: OrgId,
    /// Pre-computed throttle key from `ScheduledTarget::build`. `None` for
    /// check kinds the throttle skips (DNS, DomainExpiry — RDAP path).
    pub host_key: Option<crate::worker::host_throttle::HostKey>,
    /// Pre-computed circuit-breaker key. Cheap Arc clone — never recomputed
    /// on the dispatch hot path.
    pub breaker_key: Arc<str>,
    /// Pre-computed RDAP TLD for DomainExpiry checks.
    pub rdap_tld: Option<Arc<str>>,
}

impl CheckTask {
    pub fn breaker_key(&self) -> &str {
        &self.breaker_key
    }
}

/// Canonical circuit-breaker key for a CheckSpec. Shared between the scheduler
/// fan-out and the on-demand `check-now` handler so both paths share the
/// same per-host breaker. Hosts are normalized (lowercased + trailing-dot
/// stripped) — otherwise `Example.com` and `example.com` from two targets
/// allocate two breakers + grow the map without bound under tenant-
/// controlled casing.
pub fn host_for_spec(spec: &CheckSpec) -> String {
    if let Some((host, _)) = crate::worker::host_throttle::host_port_raw(spec) {
        return crate::worker::host_throttle::canonical_host(host);
    }
    match spec {
        // Group circuit-breaker state by TLD so a flaky registry doesn't
        // trip the breaker for unrelated TLDs.
        CheckSpec::DomainExpiry(d) => {
            let tld = d.domain.rsplit('.').next().unwrap_or("unknown");
            format!("rdap:{}", tld.to_ascii_lowercase())
        }
        // Custom resolver → key by the resolver itself; default → key by
        // the queried name so a single broken domain doesn't trip the
        // system breaker.
        CheckSpec::Dns(d) => match &d.resolver {
            Some(addr) => format!("dns:{addr}"),
            None => format!("dns:{}", d.domain.to_ascii_lowercase()),
        },
        // Passive kind, dispatched on the fast path before any breaker
        // lookup, so this key never gates.
        CheckSpec::Heartbeat(_) => "heartbeat".to_owned(),
        // host_port_raw already covered these; reaching here means an
        // HTTP URL with no host_str. Listed explicitly so a future
        // CheckSpec variant is forced through the match and can't
        // silently collapse onto this catch-all.
        CheckSpec::Http(_)
        | CheckSpec::Tcp(_)
        | CheckSpec::Ping(_)
        | CheckSpec::TlsCert(_)
        | CheckSpec::Flow(_) => "unknown".to_owned(),
    }
}

/// Sink for completed CheckResults into the storage mpsc. Notifications are
/// driven separately by the incident writer reading the result stream, so the
/// hot path no longer fans out per-result alert signals.
#[derive(Clone)]
pub struct ResultFanout {
    storage: mpsc::Sender<CheckResult>,
    storage_dropped: Arc<AtomicU64>,
}

impl ResultFanout {
    pub fn new(storage: mpsc::Sender<CheckResult>) -> Self {
        Self {
            storage,
            storage_dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn queue_depth(&self) -> u64 {
        let max = self.storage.max_capacity();
        max.saturating_sub(self.storage.capacity()) as u64
    }

    pub fn dropped(&self) -> u64 {
        self.storage_dropped.load(Ordering::Relaxed)
    }

    pub fn note_storage_dropped(&self) {
        self.storage_dropped.fetch_add(1, Ordering::Relaxed);
    }

    fn dispatch(&self, result: CheckResult) {
        if let Err(err) = self.storage.try_send(result) {
            tracing::warn!(?err, "result channel full or closed");
            counter!(metric_names::STORAGE_DROPPED, "reason" => "queue_full").increment(1);
            self.note_storage_dropped();
        }
    }
}

pub struct WorkerPool {
    semaphore: Arc<Semaphore>,
    max_concurrent: usize,
    http_clients: Arc<HttpClients>,
    breakers: Arc<DashMap<Arc<str>, Arc<CircuitBreaker>>>,
    breaker_cfg: CircuitBreakerConfig,
    fanout: ResultFanout,
    host_throttle: Arc<HostThrottle>,
    domain_expiry: Arc<crate::worker::domain_expiry::DomainExpiryRuntime>,
    /// Heartbeat ping state read by the passive executor. Owned here (not
    /// injected) so the pool and web ingest path share one instance. A split
    /// would silently evaluate stale anchors.
    heartbeat: Arc<crate::worker::heartbeat::HeartbeatRuntime>,
    /// Target ids whose probe is mid-flight. Drop-guard backed (see
    /// `InFlightGuard`) so panics also release. Bounds duplicate probes when
    /// a slow check outlasts its cadence.
    in_flight: Arc<DashSet<Uuid>>,
    /// Browser-flow engine; `None` on nodes that don't run flow.
    flow_engine: Option<Arc<crate::worker::flow::engine::CdpEngine>>,
    /// Where a flow run's trace and page snapshot go. `None` leaves the run
    /// unrecorded, which costs history and nothing else.
    flow_runs: Option<Arc<dyn crate::storage::traits::FlowRunSink>>,
}

impl WorkerPool {
    pub fn new(
        max_concurrent: usize,
        http_clients: HttpClients,
        breaker_cfg: CircuitBreakerConfig,
        fanout: ResultFanout,
        host_throttle: Arc<HostThrottle>,
        domain_expiry: Arc<crate::worker::domain_expiry::DomainExpiryRuntime>,
    ) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
            http_clients: Arc::new(http_clients),
            breakers: Arc::new(DashMap::new()),
            breaker_cfg,
            fanout,
            host_throttle,
            domain_expiry,
            heartbeat: Arc::new(crate::worker::heartbeat::HeartbeatRuntime::default()),
            in_flight: Arc::new(DashSet::new()),
            flow_engine: None,
            flow_runs: None,
        }
    }

    /// Builder-style so pools without flow keep the shorter `new` signature.
    pub fn with_flow_engine(
        mut self,
        engine: Option<Arc<crate::worker::flow::engine::CdpEngine>>,
    ) -> Self {
        self.flow_engine = engine;
        self
    }

    pub fn with_flow_runs(
        mut self,
        sink: Option<Arc<dyn crate::storage::traits::FlowRunSink>>,
    ) -> Self {
        self.flow_runs = sink;
        self
    }

    pub fn host_throttle(&self) -> Arc<HostThrottle> {
        self.host_throttle.clone()
    }

    pub fn domain_expiry_runtime(&self) -> Arc<crate::worker::domain_expiry::DomainExpiryRuntime> {
        self.domain_expiry.clone()
    }

    /// Lets the interactive (test / check-now) executor reuse the same engine.
    pub fn flow_engine(&self) -> Option<Arc<crate::worker::flow::engine::CdpEngine>> {
        self.flow_engine.clone()
    }

    pub fn heartbeat_runtime(&self) -> Arc<crate::worker::heartbeat::HeartbeatRuntime> {
        self.heartbeat.clone()
    }

    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    pub fn in_flight(&self) -> usize {
        self.max_concurrent
            .saturating_sub(self.semaphore.available_permits())
    }

    pub fn open_breakers(&self) -> usize {
        self.breakers
            .iter()
            .filter(|e| e.value().state() == BreakerState::Open)
            .count()
    }

    pub fn result_queue_depth(&self) -> u64 {
        self.fanout.queue_depth()
    }

    pub fn dropped_results(&self) -> u64 {
        self.fanout.dropped()
    }

    pub fn http_clients(&self) -> Arc<HttpClients> {
        self.http_clients.clone()
    }

    pub fn breaker_for(&self, host: &str) -> Arc<CircuitBreaker> {
        get_or_init_breaker(&self.breakers, host, self.breaker_cfg)
    }

    /// Drop breakers whose only strong reference is the map and that are
    /// in the steady `Closed` state. Run from the scheduler tick to bound
    /// the breaker map under target churn (user-driven host edits, deletes).
    pub fn sweep_breakers(&self) -> usize {
        crate::worker::sweep_idle(&self.breakers, |b| b.state() == BreakerState::Closed)
    }

    /// Runs a one-off check against `target` honoring the per-host circuit
    /// breaker. Returns `None` if the breaker is open and `force` is false.
    /// Result is recorded on the breaker but NOT dispatched through the
    /// pool's fanout — caller decides whether to persist.
    pub async fn run_once(
        &self,
        target_id: Uuid,
        org_id: Uuid,
        spec: &CheckSpec,
        host: &str,
        force: bool,
    ) -> Option<CheckResult> {
        let breaker = self.breaker_for(host);
        if !force && breaker.state() == BreakerState::Open && !breaker.allow() {
            return None;
        }
        let deps = crate::worker::WorkerDeps {
            http: &self.http_clients,
            domain_expiry: &self.domain_expiry,
            flow: self.flow_engine.as_deref(),
        };
        let result = crate::worker::execute(target_id, org_id, spec, &deps).await;
        breaker.record(result.status);
        Some(result)
    }

    pub fn dispatch(&self, task: CheckTask) {
        // Passive fast path: a heartbeat evaluation is a synchronous map read
        // with no spawn, permit, breaker, or throttle. Kept out of the probe
        // pipeline so the machinery below needs no passive special case.
        if let CheckSpec::Heartbeat(h) = &task.target.check {
            let result = crate::worker::heartbeat::execute_heartbeat_check(
                task.target.id,
                task.org_id.0,
                h,
                &self.heartbeat,
            );
            record_metrics(&result);
            self.fanout.dispatch(result);
            return;
        }
        // Skip-if-in-flight: scheduler ticks every `interval`, but a slow
        // check (or a user-configured timeout > interval) can outlast its
        // cadence. Without this guard, two probes for the same target race —
        // duplicate ClickHouse rows at near-identical timestamps, double
        // host-throttle consumption, ambiguous alert ordering.
        if !self.in_flight.insert(task.target.id) {
            counter!(metric_names::STORAGE_DROPPED, "reason" => "target_in_flight").increment(1);
            self.fanout.note_storage_dropped();
            return;
        }
        let in_flight_guard = InFlightGuard {
            set: self.in_flight.clone(),
            id: task.target.id,
        };

        let permit = match self.semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::debug!(target_id = %task.target.id, "worker pool saturated, dropping task");
                counter!(metric_names::STORAGE_DROPPED, "reason" => "pool_saturated").increment(1);
                self.fanout.note_storage_dropped();
                return;
            }
        };

        let clients = self.http_clients.clone();
        let breakers = self.breakers.clone();
        let breaker_cfg = self.breaker_cfg;
        let fanout = self.fanout.clone();
        let throttle = self.host_throttle.clone();
        let domain_expiry = self.domain_expiry.clone();
        let flow_engine = self.flow_engine.clone();
        let flow_runs = self.flow_runs.clone();
        let org_id = task.org_id;

        tokio::spawn(async move {
            let _permit = permit;
            let _in_flight_guard = in_flight_guard;
            let breaker = get_or_init_breaker(&breakers, &task.breaker_key, breaker_cfg);

            if !breaker.allow() {
                counter!(metric_names::CHECK_ERRORS, "kind" => "circuit_open").increment(1);
                let result = CheckResult::error(task.target.id, org_id.0, CIRCUIT_OPEN_REASON);
                fanout.dispatch(result);
                return;
            }

            let host_permit = match acquire_host_slot(&throttle, &task) {
                Ok(p) => p,
                Err(Throttled) => {
                    HOST_THROTTLE_DROPS_C.increment(1);
                    return;
                }
            };

            let deps = crate::worker::WorkerDeps {
                http: &clients,
                domain_expiry: &domain_expiry,
                flow: flow_engine.as_deref(),
            };
            let (result, flow_run) = crate::worker::execute_recorded(
                task.target.id,
                org_id.0,
                &task.target.check,
                &deps,
            )
            .await;
            breaker.record(result.status);
            record_metrics(&result);
            drop(host_permit);
            fanout.dispatch(result);
            // Detached so the permit is released on the verdict, not on the
            // telemetry write behind it.
            if let (Some(sink), Some(run)) = (flow_runs, flow_run) {
                tokio::spawn(async move { sink.write_runs(&[run]).await });
            }
        });
    }
}

struct InFlightGuard {
    set: Arc<DashSet<Uuid>>,
    id: Uuid,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.set.remove(&self.id);
    }
}

fn acquire_host_slot(
    throttle: &HostThrottle,
    task: &CheckTask,
) -> Result<Option<HostPermit>, Throttled> {
    // RDAP/DomainExpiry no longer pre-throttles here — the executor owns
    // that path so it can fall back to the sticky last-good cache instead
    // of letting the pool synthesize a Degraded row. `rdap_tld` is still
    // pre-computed on `ScheduledTarget` for the executor's use.
    if task.rdap_tld.is_some() {
        return Ok(None);
    }
    if let Some(key) = task.host_key.as_ref() {
        HOST_THROTTLE_WAITS_HOST.increment(1);
        return throttle.acquire(key).map(Some);
    }
    Ok(None)
}

fn get_or_init_breaker(
    breakers: &DashMap<Arc<str>, Arc<CircuitBreaker>>,
    host: &str,
    cfg: CircuitBreakerConfig,
) -> Arc<CircuitBreaker> {
    if let Some(b) = breakers.get(host) {
        return b.clone();
    }
    breakers
        .entry(Arc::from(host))
        .or_insert_with(|| Arc::new(CircuitBreaker::new(cfg)))
        .clone()
}

fn record_metrics(result: &CheckResult) {
    counter!(metric_names::CHECKS_TOTAL, "status" => result.status.as_str()).increment(1);
    histogram!(metric_names::CHECK_DURATION_MS).record(result.duration_ms as f64);
}
