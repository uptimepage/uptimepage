//! In-memory dispatch for interactive checks (test / check-now).
//!
//! Agents are outbound-only (serviceless workers, behind NAT), so the brain
//! cannot open a connection to them. Instead an agent holds a long-poll
//! ([`AdHocDispatch::claim`]); the brain hands it a check the instant one is
//! dispatched ([`AdHocDispatch::dispatch`]) and routes the result back to the
//! waiting request ([`AdHocDispatch::complete`]). No DB, no busy polling: one
//! held connection per agent, woken on demand.
//!
//! Single-process: the held claim and the originating request must live on the
//! same brain instance. Fine pre-HA; a multi-instance brain would need sticky
//! routing or a shared bus.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dashmap::DashMap;
use moka::sync::Cache;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::domain::agent_wire::{DispatchKind, DispatchedCheck, HeaderPreview};
use crate::domain::{CheckResult, CheckSpec, MAX_CHECK_TIMEOUT};
use crate::http_client::HttpClients;
use crate::storage::ResultSink;
use crate::worker::flow::engine::{BACKSTOP_GRACE, INTERACTIVE_QUEUE_LIMIT};
use crate::worker::{WorkerDeps, WorkerPool};

/// How long the brain holds an agent's claim request before returning empty so
/// the agent reconnects. Below the agent's HTTP client timeout.
pub const HOLD: Duration = Duration::from_secs(25);
/// Longest a dispatched check may sit unclaimed. Past it the region has no
/// free slot after all, so the check is withdrawn and its request told so.
pub const QUEUE_WAIT: Duration = Duration::from_secs(5);
/// Wait beyond the check's own timeout: the queue, a flow's wait for a browser
/// slot and its teardown backstop, plus 5 s for posting the result back.
const RESULT_MARGIN: Duration = Duration::from_secs(
    QUEUE_WAIT.as_secs() + INTERACTIVE_QUEUE_LIMIT.as_secs() + BACKSTOP_GRACE.as_secs() + 5,
);
/// An executor re-claims as soon as a claim returns. A check dispatched in that
/// gap queues for the next claim instead of failing as "no probe available".
const RECLAIM_GRACE: Duration = Duration::from_secs(5);
/// Checks one executor runs at once, so a check burning its whole timeout on a
/// dead host never stalls the rest of its region's interactive checks.
pub const EXECUTOR_CONCURRENCY: usize = 8;
/// Cap on queued-but-unclaimed checks per region, so a region that goes dark
/// right after a liveness check can't grow memory without bound.
const MAX_QUEUE: usize = 256;
/// Pending-waiter lifetime. Outlives the longest [`result_wait`] so a check-now
/// result still persists if the agent reports just after the request gave up,
/// and bounds any waiter that is never completed (region died, queue eviction).
const PENDING_TTL: Duration = Duration::from_secs(MAX_CHECK_TIMEOUT.as_secs() + 60);

/// How long a test/check-now request waits for its check: the check's own
/// timeout plus the dispatch overhead, so a dead host reports its timeout
/// rather than a missing agent.
pub fn result_wait(check: &CheckSpec) -> Duration {
    check.timeout() + RESULT_MARGIN
}

/// Whether a delivered result belongs in the monitor's history: only a
/// check-now, and never one turned away by a busy flow engine, since the probe's
/// own capacity says nothing about the site.
pub fn persists(kind: DispatchKind, result: &CheckResult) -> bool {
    kind == DispatchKind::CheckNow && !crate::worker::flow::engine::is_busy(result)
}

/// Delivery handle behind interior mutability so the TTL cache can hold it
/// (cache values must be `Clone`) while `complete`/`abandon` take the one-shot
/// sender out exactly once.
type Waiter = Arc<Mutex<Option<oneshot::Sender<DeliveredResult>>>>;

/// Probe result an agent reported back for a dispatched check.
pub struct DeliveredResult {
    pub result: CheckResult,
    pub response_headers_preview: Vec<HeaderPreview>,
    pub response_body_snippet: Option<String>,
    pub flow_evidence: Option<crate::domain::agent_wire::FlowEvidence>,
    pub flow_steps: Vec<crate::domain::agent_wire::StepTrace>,
}

/// Authoritative check fields, returned by [`AdHocDispatch::complete`] so the
/// caller can persist a check-now result under the real org + target.
#[derive(Clone)]
pub struct DispatchMeta {
    pub kind: DispatchKind,
    pub org_id: Uuid,
    pub target_id: Option<Uuid>,
}

/// How a dispatched check ended for the request waiting on it.
pub enum Awaited {
    Delivered(Box<DeliveredResult>),
    /// No executor claimed it in time; it was withdrawn and never runs.
    Unclaimed,
    /// Claimed (or the region closed) but no result came back in time.
    TimedOut,
}

/// Whether a region can take an interactive check right now, most available
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RegionState {
    /// An executor holds a claim, or is about to claim again.
    Live,
    /// Its executors are all running checks they claimed; a new check would
    /// wait in the queue past its own deadline.
    Busy,
    /// No executor serves it.
    Absent,
}

#[derive(Default)]
struct RegionSlot {
    queue: Mutex<VecDeque<DispatchedCheck>>,
    notify: Notify,
    holders: AtomicUsize,
    /// An executor between claims is about to claim again until then.
    reclaim_until: Mutex<Option<Instant>>,
    /// The checks an executor claimed may still be running until then.
    busy_until: Mutex<Option<Instant>>,
}

fn extend_busy(until: &Mutex<Option<Instant>>, span: Duration) {
    let at = Instant::now() + span;
    let mut until = until.lock().unwrap();
    *until = Some(until.map_or(at, |prev| prev.max(at)));
}

fn in_future(until: &Mutex<Option<Instant>>) -> bool {
    until.lock().unwrap().is_some_and(|at| Instant::now() < at)
}

struct HolderGuard<'a> {
    slot: &'a RegionSlot,
    /// Whether the executor kept a free slot and so claims again at once.
    reclaims: bool,
}

impl Drop for HolderGuard<'_> {
    fn drop(&mut self) {
        // A full claim leaves the grace alone: it may be another executor's,
        // and a stale one is caught by the queue wait.
        if self.reclaims {
            let at = Instant::now() + RECLAIM_GRACE;
            let mut reclaim = self.slot.reclaim_until.lock().unwrap();
            *reclaim = Some(reclaim.map_or(at, |prev| prev.max(at)));
        }
        self.slot.holders.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct AdHocDispatch {
    regions: DashMap<String, Arc<RegionSlot>>,
    closed: AtomicBool,
    /// `check_id` → (delivery handle, authoritative meta), TTL-bounded so a
    /// waiter that is never completed — region died, request timed out, queue
    /// eviction — can't leak, and a check-now result still persists if it lands
    /// just after the request gave up.
    pending: Cache<Uuid, (Waiter, DispatchMeta)>,
}

impl Default for AdHocDispatch {
    fn default() -> Self {
        Self::new()
    }
}

impl AdHocDispatch {
    pub fn new() -> Self {
        Self {
            regions: DashMap::new(),
            closed: AtomicBool::new(false),
            pending: Cache::builder()
                .time_to_live(PENDING_TTL)
                .max_capacity(100_000)
                .build(),
        }
    }

    fn slot(&self, region: &str) -> Arc<RegionSlot> {
        self.regions.entry(region.to_string()).or_default().clone()
    }

    /// Only a live region takes a check: an executor claims only while it has
    /// a free slot, so a queued check is picked up at once or not in time.
    pub fn region_state(&self, region: &str) -> RegionState {
        if self.closed.load(Ordering::SeqCst) {
            return RegionState::Absent;
        }
        let Some(s) = self.regions.get(region) else {
            return RegionState::Absent;
        };
        if s.holders.load(Ordering::Relaxed) > 0 || in_future(&s.reclaim_until) {
            RegionState::Live
        } else if in_future(&s.busy_until) {
            RegionState::Busy
        } else {
            RegionState::Absent
        }
    }

    pub fn region_live(&self, region: &str) -> bool {
        self.region_state(region) == RegionState::Live
    }

    /// The first of `regions` in the most available state, so a busy region
    /// answers "busy" rather than an absent one answering "no probe".
    pub fn most_available<'a>(&self, regions: &'a [String]) -> Option<&'a String> {
        regions.iter().min_by_key(|r| self.region_state(r))
    }

    /// Queue a check for `region` and return a receiver for its result. Register
    /// the waiter under `check.id`. Caller should check [`Self::region_live`]
    /// first; at the per-region cap the oldest queued check is evicted and its
    /// waiter dropped (its request then fails fast).
    pub fn dispatch(
        &self,
        region: &str,
        check: DispatchedCheck,
    ) -> oneshot::Receiver<DeliveredResult> {
        let (tx, rx) = oneshot::channel();
        let meta = DispatchMeta {
            kind: check.kind,
            org_id: check.org_id,
            target_id: check.target_id,
        };
        let slot = self.slot(region);
        {
            let mut q = slot.queue.lock().unwrap();
            // Under the queue lock, so `close` either sees this check or this
            // sees `close`.
            if self.closed.load(Ordering::SeqCst) {
                return rx;
            }
            self.pending
                .insert(check.id, (Arc::new(Mutex::new(Some(tx))), meta));
            if q.len() >= MAX_QUEUE
                && let Some(evicted) = q.pop_front()
            {
                self.pending.invalidate(&evicted.id);
            }
            q.push_back(check);
        }
        slot.notify.notify_one();
        rx
    }

    /// On shutdown: refuse new checks, drop unclaimed ones, and fail every
    /// waiting request now. An agent's result could not reach this draining
    /// process anyway, so nothing is worth holding the drain for. Meta stays,
    /// so a check-now the in-process executor finishes within the shutdown
    /// deadline still persists.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        for slot in self.regions.iter() {
            slot.queue.lock().unwrap().clear();
        }
        for (_, (waiter, _)) in self.pending.iter() {
            waiter.lock().unwrap().take();
        }
    }

    /// Pull a check nobody claimed out of its queue so it never runs for
    /// nobody, and fail its request. False when an executor already has it.
    fn withdraw(&self, region: &str, check_id: Uuid) -> bool {
        let withdrawn = self.regions.get(region).is_some_and(|slot| {
            let mut q = slot.queue.lock().unwrap();
            let before = q.len();
            q.retain(|c| c.id != check_id);
            q.len() < before
        });
        if withdrawn && let Some((waiter, _)) = self.pending.remove(&check_id) {
            waiter.lock().unwrap().take();
        }
        withdrawn
    }

    /// A request gave up. A check still queued is withdrawn. One already
    /// claimed keeps its meta, so a check-now result landing within the TTL
    /// still persists.
    pub fn abandon(&self, region: &str, check_id: Uuid) {
        if !self.withdraw(region, check_id)
            && let Some((waiter, _)) = self.pending.get(&check_id)
        {
            waiter.lock().unwrap().take();
        }
    }

    /// Wait for a dispatched check. One no executor claims within
    /// [`QUEUE_WAIT`] is withdrawn: the region had no free slot after all.
    pub async fn await_result(
        &self,
        region: &str,
        check_id: Uuid,
        mut rx: oneshot::Receiver<DeliveredResult>,
        wait: Duration,
    ) -> Awaited {
        let deadline = Instant::now() + wait;
        match tokio::time::timeout(QUEUE_WAIT, &mut rx).await {
            Ok(Ok(delivered)) => return Awaited::Delivered(Box::new(delivered)),
            Ok(Err(_)) => return Awaited::TimedOut,
            Err(_) if self.withdraw(region, check_id) => return Awaited::Unclaimed,
            Err(_) => {}
        }
        match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(delivered)) => Awaited::Delivered(Box::new(delivered)),
            _ => {
                self.abandon(region, check_id);
                Awaited::TimedOut
            }
        }
    }

    /// Held long-poll: return up to `limit` checks for `region`, waiting up to
    /// [`HOLD`] for one to appear. Empty on timeout. Counts as a live holder
    /// for the duration.
    pub async fn claim(&self, region: &str, limit: usize) -> Vec<DispatchedCheck> {
        let slot = self.slot(region);
        slot.holders.fetch_add(1, Ordering::Relaxed);
        let mut guard = HolderGuard {
            slot: &slot,
            reclaims: true,
        };
        let deadline = Instant::now() + HOLD;
        loop {
            {
                let mut q = slot.queue.lock().unwrap();
                if !q.is_empty() {
                    let limit = limit.max(1);
                    let n = q.len().min(limit);
                    let claimed: Vec<_> = q.drain(..n).collect();
                    guard.reclaims = n < limit;
                    if let Some(busy) = claimed.iter().map(|c| result_wait(&c.spec)).max() {
                        extend_busy(&slot.busy_until, busy);
                    }
                    return claimed;
                }
            }
            let notified = slot.notify.notified();
            let now = Instant::now();
            if now >= deadline {
                return Vec::new();
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(deadline - now) => return Vec::new(),
            }
        }
    }

    /// Deliver a result to the waiting request (if it is still waiting) and
    /// return the check's authoritative meta so a check-now result can be
    /// persisted regardless. `None` only if already completed or TTL-expired.
    pub fn complete(&self, check_id: Uuid, result: DeliveredResult) -> Option<DispatchMeta> {
        let (waiter, meta) = self.pending.remove(&check_id)?;
        if let Some(tx) = waiter.lock().unwrap().take() {
            let _ = tx.send(result);
        }
        Some(meta)
    }
}

/// Claim→run loop shared by every executor. Claims no more checks than it has
/// free slots and runs each on its own task, so one slow check never holds up
/// the checks claimed beside it or blocks the next claim. Cancel is seen
/// between claims, so `claim` must return within its hold.
pub async fn serve_claims<C, CF, R, RF>(claim: C, run: R, cancel: CancellationToken)
where
    C: Fn(usize) -> CF,
    CF: Future<Output = Vec<DispatchedCheck>>,
    R: Fn(DispatchedCheck) -> RF,
    RF: Future<Output = ()> + Send + 'static,
{
    let slots = Arc::new(Semaphore::new(EXECUTOR_CONCURRENCY));
    let mut running = JoinSet::new();
    while let Some(first) = free_slot(&slots, &cancel).await {
        let mut free = vec![first];
        while let Ok(more) = slots.clone().try_acquire_owned() {
            free.push(more);
        }
        // Not raced against cancel: a claim answered in flight would drop
        // checks the brain has already handed over.
        let claimed = claim(free.len()).await;
        for check in claimed {
            // A brain that ignores the limit can over-deliver; the extra
            // checks wait for a slot, even through shutdown, since they are
            // already off the brain's queue.
            let Some(slot) = (match free.pop() {
                Some(slot) => Some(slot),
                None => slots.clone().acquire_owned().await.ok(),
            }) else {
                break;
            };
            let task = run(check);
            running.spawn(async move {
                task.await;
                drop(slot);
            });
        }
        while running.try_join_next().is_some() {}
    }
    // Stop claiming but let claimed checks finish, so an agent still reports
    // them to a live brain. The brain's own executor is cut off by its
    // shutdown deadline.
    while running.join_next().await.is_some() {}
}

async fn free_slot(
    slots: &Arc<Semaphore>,
    cancel: &CancellationToken,
) -> Option<OwnedSemaphorePermit> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        p = slots.clone().acquire_owned() => p.ok(),
    }
}

/// All-in-one executor: serve this brain's own region in-process so a
/// self-hosted single process (no separate agent) still runs test / check-now.
/// Mirrors the agent's claim→execute→complete loop but talks to the local
/// dispatch directly and persists check-now via the local sink. Spawned only
/// when the in-process scheduler is enabled (the same "this process probes its
/// region" switch); a pure control plane leaves this to agents.
pub async fn run_local_executor(
    dispatch: Arc<AdHocDispatch>,
    region: String,
    worker_pool: Arc<WorkerPool>,
    http_clients: Arc<HttpClients>,
    result_sink: Arc<dyn ResultSink>,
    cancel: CancellationToken,
) {
    // In-process, a claim hands over nothing until it returns, so dropping it
    // on cancel loses no check.
    let claim = |limit| {
        let (dispatch, region, cancel) = (&dispatch, &region, &cancel);
        async move {
            tokio::select! {
                _ = cancel.cancelled() => Vec::new(),
                c = dispatch.claim(region, limit) => c,
            }
        }
    };
    let run = |check| {
        run_local_check(
            dispatch.clone(),
            worker_pool.clone(),
            http_clients.clone(),
            result_sink.clone(),
            check,
        )
    };
    serve_claims(claim, run, cancel.clone()).await;
}

async fn run_local_check(
    dispatch: Arc<AdHocDispatch>,
    worker_pool: Arc<WorkerPool>,
    http_clients: Arc<HttpClients>,
    result_sink: Arc<dyn ResultSink>,
    check: DispatchedCheck,
) {
    let domain_runtime = worker_pool.domain_expiry_runtime();
    let flow_engine = worker_pool.flow_engine();
    let deps = WorkerDeps {
        http: &http_clients,
        domain_expiry: &domain_runtime,
        flow: flow_engine.as_deref(),
    };
    let target_id = check.target_id.unwrap_or_else(Uuid::nil);
    let (result, probe) =
        crate::worker::execute_with_probe(target_id, check.org_id, &check.spec, &deps).await;
    // We produced the result with the authoritative ids, so persist it as-is
    // (mirrors the agent result-ingest path).
    let persist = persists(check.kind, &result).then(|| result.clone());
    dispatch.complete(
        check.id,
        DeliveredResult {
            result,
            response_headers_preview: probe.response_headers_preview,
            response_body_snippet: probe.response_body_snippet,
            flow_evidence: probe.flow_evidence,
            flow_steps: probe.flow_steps,
        },
    );
    if let Some(r) = persist
        && let Err(err) = result_sink.write_batch(std::slice::from_ref(&r)).await
    {
        tracing::warn!(error = %err, "in-process check-now persist failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CheckSpec, CheckStatus};
    use chrono::Utc;

    fn spec() -> CheckSpec {
        serde_json::from_value(serde_json::json!({
            "type": "http", "url": "https://example.com/", "method": "GET",
            "timeout": 5000, "follow_redirects": true, "max_redirects": 5,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {}, "verify_tls": true
        }))
        .unwrap()
    }

    fn dispatched(id: Uuid) -> DispatchedCheck {
        DispatchedCheck {
            id,
            kind: DispatchKind::Test,
            org_id: Uuid::nil(),
            target_id: None,
            spec: spec(),
        }
    }

    fn result() -> DeliveredResult {
        DeliveredResult {
            result: CheckResult {
                target_id: Uuid::nil(),
                org_id: Uuid::nil(),
                timestamp: Utc::now(),
                status: CheckStatus::Up,
                duration_ms: 1,
                dns_ms: None,
                connect_ms: None,
                tls_ms: None,
                ttfb_ms: None,
                response_code: Some(200),
                response_size: None,
                diagnostic: None,
                error: None,
            },
            response_headers_preview: vec![],
            response_body_snippet: None,
            flow_evidence: None,
            flow_steps: vec![],
        }
    }

    #[tokio::test]
    async fn region_dead_until_a_holder_claims() {
        let d = AdHocDispatch::new();
        assert!(!d.region_live("r"));
        let d = std::sync::Arc::new(d);
        let h = {
            let d = d.clone();
            tokio::spawn(async move { d.claim("r", 8).await })
        };
        // Let the holder register.
        for _ in 0..50 {
            if d.region_live("r") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(d.region_live("r"));
        // No check → holder times out empty. Speed not asserted (HOLD is 25s);
        // just confirm it eventually drops the holder.
        h.abort();
    }

    #[tokio::test]
    async fn dispatch_handoff_and_complete_round_trip() {
        let d = std::sync::Arc::new(AdHocDispatch::new());
        let holder = {
            let d = d.clone();
            tokio::spawn(async move { d.claim("r", 8).await })
        };
        for _ in 0..50 {
            if d.region_live("r") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let id = Uuid::now_v7();
        let rx = d.dispatch("r", dispatched(id));
        let claimed = holder.await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, id);

        let meta = d.complete(id, result()).expect("waiter present");
        assert_eq!(meta.kind, DispatchKind::Test);
        let delivered = rx.await.expect("result delivered");
        assert_eq!(delivered.result.status, CheckStatus::Up);

        // Second complete is a no-op (waiter consumed).
        assert!(d.complete(id, result()).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn region_stays_live_between_claims_then_dies() {
        let d = AdHocDispatch::new();
        let claimed = d.claim("r", 8);
        tokio::pin!(claimed);
        tokio::select! {
            biased;
            _ = &mut claimed => unreachable!("empty queue holds until HOLD"),
            _ = tokio::task::yield_now() => {}
        }
        assert!(d.region_live("r"));
        assert!(claimed.await.is_empty());
        assert!(
            d.region_live("r"),
            "a check sent before the re-claim queues"
        );
        tokio::time::advance(RECLAIM_GRACE).await;
        assert!(!d.region_live("r"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_region_running_what_it_claimed_is_busy_not_live() {
        let d = AdHocDispatch::new();
        let mut check = dispatched(Uuid::now_v7());
        let CheckSpec::Http(http) = &mut check.spec else {
            unreachable!()
        };
        http.timeout = Duration::from_secs(60);
        let wait = result_wait(&check.spec);
        let _rx = d.dispatch("r", check);
        assert_eq!(d.claim("r", 8).await.len(), 1);
        assert_eq!(d.region_state("r"), RegionState::Live);
        tokio::time::advance(RECLAIM_GRACE * 2).await;
        assert_eq!(
            d.region_state("r"),
            RegionState::Busy,
            "no free slot, so a new check would expire in the queue"
        );
        tokio::time::advance(wait).await;
        assert_eq!(d.region_state("r"), RegionState::Absent);
    }

    #[test]
    fn only_a_check_now_that_reached_the_target_persists() {
        let mut busy = result().result;
        busy.status = CheckStatus::Error;
        busy.error = Some(
            "every browser slot stayed busy with other flows for 10 s; try again shortly".into(),
        );
        assert!(persists(DispatchKind::CheckNow, &result().result));
        assert!(!persists(DispatchKind::CheckNow, &busy));
        assert!(!persists(DispatchKind::Test, &result().result));
    }

    #[test]
    fn result_wait_outlasts_the_checks_own_timeout() {
        let mut check = spec();
        let CheckSpec::Http(http) = &mut check else {
            unreachable!()
        };
        http.timeout = Duration::from_secs(60);
        assert!(result_wait(&check) > Duration::from_secs(60));
        assert!(result_wait(&check) < PENDING_TTL);
    }

    #[tokio::test]
    async fn a_slow_check_does_not_block_the_next_claim() {
        let (slow, fast) = (Uuid::now_v7(), Uuid::now_v7());
        let batches = Arc::new(Mutex::new(VecDeque::from([
            vec![dispatched(slow)],
            vec![dispatched(fast)],
        ])));
        let limits = Arc::new(Mutex::new(Vec::new()));
        let cancel = CancellationToken::new();
        let claim = |limit| {
            limits.lock().unwrap().push(limit);
            let next = batches.lock().unwrap().pop_front();
            let cancel = cancel.clone();
            async move {
                match next {
                    Some(batch) => batch,
                    None => {
                        cancel.cancelled().await;
                        Vec::new()
                    }
                }
            }
        };
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        let run = |check: DispatchedCheck| {
            let done_tx = done_tx.clone();
            let release = release.clone();
            async move {
                if check.id == slow {
                    release.notified().await;
                }
                let _ = done_tx.send(check.id);
            }
        };
        let serving = serve_claims(claim, run, cancel.clone());
        tokio::pin!(serving);
        let ran = tokio::select! {
            _ = &mut serving => unreachable!("serves until cancelled"),
            id = done_rx.recv() => id,
        };
        assert_eq!(ran, Some(fast));
        assert_eq!(
            limits.lock().unwrap()[..2],
            [EXECUTOR_CONCURRENCY, EXECUTOR_CONCURRENCY - 1],
            "claims only as many checks as it has free slots"
        );
        cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut serving)
                .await
                .is_err(),
            "cancel waits for the checks already running"
        );
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), serving)
            .await
            .expect("the loop ends once its running checks finish");
        assert_eq!(done_rx.recv().await, Some(slow));
    }

    #[tokio::test]
    async fn close_fails_every_waiting_request_but_keeps_meta() {
        let d = AdHocDispatch::new();
        let (claimed, queued) = (Uuid::now_v7(), Uuid::now_v7());
        let claimed_rx = d.dispatch("r", dispatched(claimed));
        assert_eq!(d.claim("r", 1).await.len(), 1);
        let queued_rx = d.dispatch("r", dispatched(queued));
        d.close();
        assert!(queued_rx.await.is_err(), "an unclaimed check fails at once");
        assert!(!d.region_live("r"));
        assert!(d.dispatch("r", dispatched(Uuid::now_v7())).await.is_err());
        assert!(
            claimed_rx.await.is_err(),
            "a claimed check's request fails too"
        );
        assert!(
            d.complete(claimed, result()).is_some(),
            "a finished check-now still persists"
        );
    }

    #[tokio::test]
    async fn abandon_withdraws_a_check_nobody_claimed() {
        let d = AdHocDispatch::new();
        let id = Uuid::now_v7();
        let rx = d.dispatch("r", dispatched(id));
        d.abandon("r", id);
        let closed = tokio::time::timeout(Duration::from_secs(1), rx).await;
        assert!(closed.expect("sender dropped at once").is_err());
        assert!(d.slot("r").queue.lock().unwrap().is_empty(), "never runs");
        assert!(d.complete(id, result()).is_none(), "nothing to persist");
    }

    #[tokio::test(start_paused = true)]
    async fn a_claim_that_fills_every_free_slot_leaves_the_region_busy() {
        let d = AdHocDispatch::new();
        let _rx = d.dispatch("r", dispatched(Uuid::now_v7()));
        assert_eq!(d.claim("r", 1).await.len(), 1);
        assert_eq!(
            d.region_state("r"),
            RegionState::Busy,
            "no reclaim grace for an executor with no free slot"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_claim_keeps_another_executors_grace() {
        let d = AdHocDispatch::new();
        assert!(d.claim("r", 8).await.is_empty(), "hold expires empty");
        let _rx = d.dispatch("r", dispatched(Uuid::now_v7()));
        assert_eq!(d.claim("r", 1).await.len(), 1);
        assert_eq!(d.region_state("r"), RegionState::Live);
    }

    #[tokio::test(start_paused = true)]
    async fn a_busy_region_is_preferred_over_an_absent_one() {
        let d = AdHocDispatch::new();
        let _rx = d.dispatch("busy", dispatched(Uuid::now_v7()));
        assert_eq!(d.claim("busy", 1).await.len(), 1);
        let regions = ["absent".to_string(), "busy".to_string()];
        assert_eq!(d.most_available(&regions).unwrap(), "busy");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unclaimed_check_is_withdrawn_after_the_queue_wait() {
        let d = AdHocDispatch::new();
        let id = Uuid::now_v7();
        let rx = d.dispatch("r", dispatched(id));
        let awaited = d.await_result("r", id, rx, Duration::from_secs(60)).await;
        assert!(matches!(awaited, Awaited::Unclaimed));
        assert!(d.slot("r").queue.lock().unwrap().is_empty(), "never runs");
        assert!(d.complete(id, result()).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_claimed_check_gets_the_rest_of_its_wait() {
        let d = Arc::new(AdHocDispatch::new());
        let id = Uuid::now_v7();
        let rx = d.dispatch("r", dispatched(id));
        assert_eq!(d.claim("r", 8).await.len(), 1);
        let deliver = {
            let d = d.clone();
            tokio::spawn(async move {
                tokio::time::sleep(QUEUE_WAIT * 3).await;
                d.complete(id, result());
            })
        };
        let awaited = d.await_result("r", id, rx, QUEUE_WAIT * 4).await;
        assert!(matches!(awaited, Awaited::Delivered(_)));
        deliver.await.unwrap();
    }

    #[tokio::test]
    async fn abandon_after_claim_keeps_meta_for_late_persist() {
        let d = AdHocDispatch::new();
        let id = Uuid::now_v7();
        let rx = d.dispatch("r", dispatched(id));
        assert_eq!(d.claim("r", 1).await.len(), 1);
        d.abandon("r", id);
        assert!(rx.await.is_err(), "delivery sender dropped on abandon");
        assert!(d.complete(id, result()).is_some());
        assert!(d.complete(id, result()).is_none());
    }
}
