use crate::common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uptimepage::domain::{CheckStatus, OrgId, Target};
use uptimepage::error::{AppError, Result as SmResult};
use uptimepage::pipeline::{BatcherConfig, ResultBatcher};
use uptimepage::scheduler::{Scheduler, TargetRegistry};
use uptimepage::storage::admin::EnabledTargetSource;
use uptimepage::storage::{InMemorySink, InMemoryTargetStore};
use uptimepage::worker::circuit_breaker::CIRCUIT_OPEN_REASON;
use uptimepage::worker::{ResultFanout, WorkerPool};

use crate::common::{breaker_cfg, http_target, scheduler_cfg, spawn_router, test_client};

async fn spawn_counting_mock() -> (std::net::SocketAddr, Arc<AtomicU32>) {
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    let app = Router::new().route(
        "/ping",
        get(move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::Relaxed);
                "pong"
            }
        }),
    );
    (spawn_router(app).await, counter)
}

async fn spawn_always_500() -> std::net::SocketAddr {
    let app = Router::new().route(
        "/",
        get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    spawn_router(app).await
}

#[tokio::test]
async fn scheduler_runs_target_periodically() {
    let (addr, counter) = spawn_counting_mock().await;
    let store = Arc::new(InMemoryTargetStore::from_vec(vec![http_target(
        addr, "/ping", 200,
    )]));
    let registry = Arc::new(TargetRegistry::new(store));
    let (tx, mut rx) = mpsc::channel(64);

    let pool = Arc::new(WorkerPool::new(
        50,
        test_client(),
        breaker_cfg(),
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        common::test_domain_expiry_runtime(),
    ));
    let scheduler = Arc::new(Scheduler::new(registry, pool, scheduler_cfg(30)));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.clone().run(shutdown.clone()));

    let mut received = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1200);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(r)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            assert_eq!(r.status, CheckStatus::Up);
            received += 1;
            if received >= 3 {
                break;
            }
        }
    }
    shutdown.cancel();
    handle.await.unwrap().unwrap();

    assert!(received >= 3, "expected ≥3 results, got {received}");
    assert!(counter.load(Ordering::Relaxed) >= 3);
}

// Coverage smoke for the stagger code path: confirms the deterministic
// phase-offset scheduling still produces results promptly. Cadence-steadiness
// is locked structurally by the `stagger_offset` unit tests (bounded,
// deterministic); this test asserts only that the driver keeps ticking, not
// exact wall-clock timing.
#[tokio::test]
async fn scheduler_runs_staggered_target() {
    let (addr, counter) = spawn_counting_mock().await;
    let interval = Duration::from_millis(300);
    let store = Arc::new(InMemoryTargetStore::from_vec(vec![http_target(
        addr,
        "/ping",
        interval.as_millis() as u64,
    )]));
    let registry = Arc::new(TargetRegistry::new(store));
    let (tx, mut rx) = mpsc::channel(64);

    let pool = Arc::new(WorkerPool::new(
        50,
        test_client(),
        breaker_cfg(),
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        common::test_domain_expiry_runtime(),
    ));
    let scheduler = Arc::new(Scheduler::new(registry, pool, scheduler_cfg(30)));
    let shutdown = CancellationToken::new();
    let start = tokio::time::Instant::now();
    let handle = tokio::spawn(scheduler.clone().run(shutdown.clone()));

    let mut received = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while tokio::time::Instant::now() < deadline && received < 3 {
        if let Ok(Some(r)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            assert_eq!(r.status, CheckStatus::Up);
            received += 1;
        }
    }
    let elapsed = start.elapsed();
    shutdown.cancel();
    handle.await.unwrap().unwrap();

    assert!(received >= 3, "expected ≥3 results, got {received}");
    // First check is immediate, the rest one interval apart: 3 results must
    // arrive well inside a window that a stalled/never-firing loop would miss.
    assert!(
        elapsed < interval * 8,
        "3 staggered checks took {elapsed:?}, expected < {:?}",
        interval * 8,
    );
    assert!(
        counter.load(Ordering::Relaxed) >= 3,
        "mock should have served ≥3 pings"
    );
}

async fn spawn_slow_mock(delay: Duration) -> (std::net::SocketAddr, Arc<AtomicU32>) {
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    let app = Router::new().route(
        "/slow",
        get(move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(delay).await;
                "ok"
            }
        }),
    );
    (spawn_router(app).await, counter)
}

#[tokio::test]
async fn dispatch_skips_when_target_probe_already_in_flight() {
    // Cadence 150 ms, response time 600 ms — three ticks land before the
    // first probe completes. Without the in-flight guard the mock would see
    // every tick; with it, ticks 2-N drop and the counter only advances per
    // completed probe.
    let response = Duration::from_millis(600);
    let cadence = Duration::from_millis(150);
    let (addr, counter) = spawn_slow_mock(response).await;
    let store = Arc::new(InMemoryTargetStore::from_vec(vec![http_target(
        addr,
        "/slow",
        cadence.as_millis() as u64,
    )]));
    let registry = Arc::new(TargetRegistry::new(store));
    let (tx, _rx) = mpsc::channel(64);

    let pool = Arc::new(WorkerPool::new(
        50,
        test_client(),
        breaker_cfg(),
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        common::test_domain_expiry_runtime(),
    ));
    let scheduler = Arc::new(Scheduler::new(registry, pool.clone(), scheduler_cfg(30)));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.clone().run(shutdown.clone()));

    tokio::time::sleep(Duration::from_millis(1_500)).await;
    shutdown.cancel();
    handle.await.unwrap().unwrap();

    let probes = counter.load(Ordering::Relaxed);
    let dropped = pool.dropped_results();
    // Upper bound (cadence-only): 1500/150 = 10. With the in-flight guard
    // the worker can only start one probe per ~600 ms ⇒ ≤ 3.
    assert!(
        probes <= 4,
        "probe should be deduplicated; got {probes} (expected ≤ 4)"
    );
    assert!(
        dropped >= 3,
        "in-flight dispatches should drop; got {dropped} (expected ≥ 3)"
    );
}

#[tokio::test]
async fn scheduler_picks_up_new_targets_on_refresh() {
    let (addr, counter) = spawn_counting_mock().await;
    let store = Arc::new(InMemoryTargetStore::from_vec(vec![]));
    let registry = Arc::new(TargetRegistry::new(store.clone()));
    let (tx, mut rx) = mpsc::channel(64);

    let pool = Arc::new(WorkerPool::new(
        50,
        test_client(),
        breaker_cfg(),
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        common::test_domain_expiry_runtime(),
    ));
    let scheduler = Arc::new(Scheduler::new(registry, pool, scheduler_cfg(1)));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.clone().run(shutdown.clone()));

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(scheduler.active_tasks(), 0);

    store.insert(http_target(addr, "/ping", 200));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut got = false;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            got = true;
            break;
        }
    }
    shutdown.cancel();
    handle.await.unwrap().unwrap();
    assert!(got, "no result after adding target");
    assert!(counter.load(Ordering::Relaxed) >= 1);
}

#[tokio::test]
async fn shutdown_drains_in_flight_results() {
    let (addr, counter) = spawn_counting_mock().await;
    let store = Arc::new(InMemoryTargetStore::from_vec(vec![http_target(
        addr, "/ping", 100,
    )]));
    let registry = Arc::new(TargetRegistry::new(store));
    let (tx, rx) = mpsc::channel(64);
    let sink = Arc::new(InMemorySink::new());

    let pool = Arc::new(WorkerPool::new(
        50,
        test_client(),
        breaker_cfg(),
        ResultFanout::new(tx.clone()),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        common::test_domain_expiry_runtime(),
    ));
    let scheduler = Arc::new(Scheduler::new(registry, pool, scheduler_cfg(30)));
    let batcher = ResultBatcher::new(
        rx,
        sink.clone(),
        BatcherConfig {
            batch_size: 1000,
            batch_timeout: Duration::from_secs(60),
        },
    );
    let shutdown = CancellationToken::new();
    let scheduler_handle = tokio::spawn(scheduler.run(shutdown.clone()));
    let batcher_handle = tokio::spawn(batcher.run(shutdown.clone()));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline && counter.load(Ordering::Relaxed) < 1 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(counter.load(Ordering::Relaxed) >= 1, "no checks ran");
    assert_eq!(sink.len(), 0, "batcher should not have flushed yet");

    shutdown.cancel();
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), scheduler_handle)
        .await
        .expect("scheduler did not exit")
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), batcher_handle)
        .await
        .expect("batcher did not exit")
        .unwrap();

    assert!(
        !sink.is_empty(),
        "shutdown should drain in-flight results, got {}",
        sink.len()
    );
}

struct FailingThenSucceedingSource {
    inner: Arc<InMemoryTargetStore>,
    remaining_failures: AtomicU32,
}

impl FailingThenSucceedingSource {
    fn new(inner: Arc<InMemoryTargetStore>, fail_count: u32) -> Self {
        Self {
            inner,
            remaining_failures: AtomicU32::new(fail_count),
        }
    }

    fn remaining(&self) -> u32 {
        self.remaining_failures.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl EnabledTargetSource for FailingThenSucceedingSource {
    async fn list_all_enabled_targets(&self) -> SmResult<Vec<(OrgId, Target)>> {
        if self.remaining_failures.load(Ordering::SeqCst) > 0 {
            self.remaining_failures.fetch_sub(1, Ordering::SeqCst);
            return Err(AppError::Other(anyhow::anyhow!(
                "test: simulated registry refresh failure",
            )));
        }
        self.inner.list_all_enabled_targets().await
    }
}

#[tokio::test]
async fn scheduler_recovers_after_transient_refresh_failures() {
    // Source fails 2 times then succeeds. With base 1s and the backoff
    // schedule 1s, 2s, ..., recovery lands no later than ~3s from boot —
    // a 30s deadline gives generous slack for CI scheduling.
    let (addr, _counter) = spawn_counting_mock().await;
    let inner = Arc::new(InMemoryTargetStore::from_vec(vec![http_target(
        addr, "/ping", 200,
    )]));
    let source = Arc::new(FailingThenSucceedingSource::new(inner.clone(), 2));
    let registry = Arc::new(TargetRegistry::new(source.clone()));
    let (tx, mut rx) = mpsc::channel(64);

    let pool = Arc::new(WorkerPool::new(
        50,
        test_client(),
        breaker_cfg(),
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        common::test_domain_expiry_runtime(),
    ));
    let scheduler = Arc::new(Scheduler::new(registry, pool, scheduler_cfg(1)));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.run(shutdown.clone()));

    // Wait for the first successful dispatch's result to arrive. If the
    // scheduler bailed out on the first failure (no recovery), this hangs
    // until the timeout fires.
    let got = tokio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .ok()
        .flatten();

    shutdown.cancel();
    handle.await.unwrap().unwrap();

    assert!(
        got.is_some(),
        "scheduler never recovered to dispatch a check"
    );
    assert_eq!(
        source.remaining(),
        0,
        "all scheduled failures must have been consumed by the scheduler",
    );
}

#[tokio::test]
async fn scheduler_shutdown_observed_during_slow_refresh() {
    // Source that simulates a slow PG handshake: blocks indefinitely on
    // each list_all call. Without cancel-aware tick_once, shutdown would
    // be blocked waiting for the in-flight refresh.
    struct HangingSource;
    #[async_trait]
    impl EnabledTargetSource for HangingSource {
        async fn list_all_enabled_targets(&self) -> SmResult<Vec<(OrgId, Target)>> {
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    let registry = Arc::new(TargetRegistry::new(Arc::new(HangingSource)));
    let (tx, _rx) = mpsc::channel(64);
    let pool = Arc::new(WorkerPool::new(
        10,
        test_client(),
        breaker_cfg(),
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        common::test_domain_expiry_runtime(),
    ));
    let scheduler = Arc::new(Scheduler::new(registry, pool, scheduler_cfg(1)));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.run(shutdown.clone()));

    // Give the initial tick_once a beat to enter its blocking await.
    tokio::time::sleep(Duration::from_millis(50)).await;
    shutdown.cancel();

    let started = std::time::Instant::now();
    let res = tokio::time::timeout(Duration::from_secs(2), handle).await;
    let elapsed = started.elapsed();

    let join_outcome =
        res.expect("scheduler did not return within 2s — cancel-aware tick_once broken");
    join_outcome.unwrap().unwrap();
    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown took {elapsed:?}, expected < 2s",
    );
}

#[tokio::test]
async fn worker_pool_breaker_opens_after_failures() {
    let addr = spawn_always_500().await;
    let store = Arc::new(InMemoryTargetStore::from_vec(vec![http_target(
        addr, "/", 100,
    )]));
    let registry = Arc::new(TargetRegistry::new(store));
    let (tx, mut rx) = mpsc::channel(64);

    let pool = Arc::new(WorkerPool::new(
        50,
        test_client(),
        breaker_cfg(),
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        common::test_domain_expiry_runtime(),
    ));
    let scheduler = Arc::new(Scheduler::new(registry, pool.clone(), scheduler_cfg(30)));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.run(shutdown.clone()));

    let mut downs = 0;
    let mut circuit_opens = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline && circuit_opens == 0 {
        if let Ok(Some(r)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            match r.status {
                CheckStatus::Down => downs += 1,
                CheckStatus::Error if r.error.as_deref() == Some(CIRCUIT_OPEN_REASON) => {
                    circuit_opens += 1
                }
                _ => {}
            }
        }
    }
    shutdown.cancel();
    handle.await.unwrap().unwrap();

    assert!(downs >= 2, "expected ≥2 downs, got {downs}");
    assert!(circuit_opens >= 1, "breaker never opened");
    assert!(pool.open_breakers() >= 1);
}

struct MutableSource {
    targets: std::sync::Mutex<Vec<(OrgId, Target)>>,
}

impl MutableSource {
    fn new(initial: Vec<(OrgId, Target)>) -> Self {
        Self {
            targets: std::sync::Mutex::new(initial),
        }
    }

    fn set(&self, targets: Vec<(OrgId, Target)>) {
        *self.targets.lock().unwrap() = targets;
    }
}

#[async_trait]
impl EnabledTargetSource for MutableSource {
    async fn list_all_enabled_targets(&self) -> SmResult<Vec<(OrgId, Target)>> {
        Ok(self.targets.lock().unwrap().clone())
    }
}

#[tokio::test]
async fn scheduler_applies_update_and_removal() {
    let (addr, _counter) = spawn_counting_mock().await;
    let org = OrgId(uuid::Uuid::from_u128(1));
    let target = http_target(addr, "/ping", 200);
    let source = Arc::new(MutableSource::new(vec![(org, target.clone())]));
    let registry = Arc::new(TargetRegistry::new(source.clone()));
    let (tx, mut rx) = mpsc::channel(64);

    let pool = Arc::new(WorkerPool::new(
        50,
        test_client(),
        breaker_cfg(),
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        common::test_domain_expiry_runtime(),
    ));
    let scheduler = Arc::new(Scheduler::new(registry, pool, scheduler_cfg(1)));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.clone().run(shutdown.clone()));

    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(matches!(first, Ok(Some(_))), "no probe after add");
    assert_eq!(scheduler.active_tasks(), 1);

    // An edit bumps updated_at → the registry reports it as `updated`. The
    // superseded heap entry must not leave a second live schedule.
    let mut updated = target.clone();
    updated.updated_at = chrono::Utc::now();
    source.set(vec![(org, updated)]);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(
        scheduler.active_tasks(),
        1,
        "update must not double-schedule the target"
    );
    // Drain any probes buffered during the sleep, then confirm probing continues.
    while rx.try_recv().is_ok() {}
    let after_update = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(
        matches!(after_update, Ok(Some(_))),
        "probing stopped after update"
    );

    source.set(vec![]);
    let reaped = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if scheduler.active_tasks() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        reaped.is_ok(),
        "removed target never dropped from the count"
    );

    shutdown.cancel();
    handle.await.unwrap().unwrap();
}
