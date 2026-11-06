use crate::common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uptimepage::agent::{AgentPullSource, MAX_PULL_BYTES};
use uptimepage::domain::agent_wire::{AgentTargetDto, AgentTargetsResponse};
use uptimepage::domain::{
    CheckResult, CheckSpec, CheckStatus, OrgId, Target, humanize_check_error,
};
use uptimepage::error::{AppError, Result as SmResult};
use uptimepage::pipeline::{BatcherConfig, ResultBatcher};
use uptimepage::scheduler::{Scheduler, TargetRegistry, stop_probing};
use uptimepage::storage::admin::EnabledTargetSource;
use uptimepage::storage::{InMemorySink, InMemoryTargetStore};
use uptimepage::worker::{ResultFanout, WorkerPool};

use crate::common::{breaker_cfg, http_target, scheduler_cfg, spawn_router, test_client};

fn test_pool(tx: mpsc::Sender<CheckResult>) -> Arc<WorkerPool> {
    Arc::new(WorkerPool::new(
        50,
        test_client(),
        breaker_cfg(),
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        common::test_domain_expiry_runtime(),
    ))
}

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

    let pool = test_pool(tx);
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

// Coverage smoke for slot scheduling: confirms the driver keeps producing
// results promptly. Cadence is locked by the `until_slot` unit tests; this
// test asserts only that the driver keeps ticking, not exact wall-clock
// timing.
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

    let pool = test_pool(tx);
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
    // First check within half an interval, the rest one interval apart: 3
    // results must arrive well inside a window a stalled loop would miss.
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
    // every tick; with it, ticks 2-N are skipped and the counter only advances
    // per completed probe.
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

    let pool = test_pool(tx);
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
        (1..=4).contains(&probes),
        "probe should run and be deduplicated; got {probes} (expected 1..=4)"
    );
    assert_eq!(
        dropped, 0,
        "a skipped tick loses no result, so it must not count as a drop"
    );
}

#[tokio::test]
async fn scheduler_picks_up_new_targets_on_refresh() {
    let (addr, counter) = spawn_counting_mock().await;
    let store = Arc::new(InMemoryTargetStore::from_vec(vec![]));
    let registry = Arc::new(TargetRegistry::new(store.clone()));
    let (tx, mut rx) = mpsc::channel(64);

    let pool = test_pool(tx);
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

struct Probing {
    pool: Arc<WorkerPool>,
    sink: Arc<InMemorySink>,
    root: CancellationToken,
    flush: CancellationToken,
    scheduler: JoinHandle<()>,
    batcher: JoinHandle<()>,
}

/// Wired as the agent and the brain wire it, returned once a check against
/// `addr` is running.
async fn mid_check(addr: std::net::SocketAddr, arrived: &AtomicU32) -> Probing {
    let store = Arc::new(InMemoryTargetStore::from_vec(vec![http_target(
        addr, "/slow", 100,
    )]));
    let (tx, rx) = mpsc::channel(64);
    let sink = Arc::new(InMemorySink::new());
    let pool = test_pool(tx);
    let scheduler = Arc::new(Scheduler::new(
        Arc::new(TargetRegistry::new(store)),
        pool.clone(),
        scheduler_cfg(30),
    ));
    let batcher = ResultBatcher::new(
        rx,
        sink.clone(),
        BatcherConfig {
            batch_size: 1000,
            batch_timeout: Duration::from_secs(60),
        },
    );
    let root = CancellationToken::new();
    let flush = CancellationToken::new();
    let scheduler = {
        let token = root.clone();
        tokio::spawn(async move { scheduler.run(token).await.unwrap() })
    };
    let batcher = tokio::spawn(batcher.run(flush.clone()));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while arrived.load(Ordering::Relaxed) == 0 {
        assert!(tokio::time::Instant::now() < deadline, "no check started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Probing {
        pool,
        sink,
        root,
        flush,
        scheduler,
        batcher,
    }
}

#[tokio::test]
async fn a_check_running_at_shutdown_still_reaches_storage() {
    let (addr, arrived) = spawn_slow_mock(Duration::from_millis(500)).await;
    let p = mid_check(addr, &arrived).await;

    stop_probing(
        &p.root,
        p.scheduler,
        &p.pool,
        Duration::from_secs(5),
        p.flush,
        p.batcher,
    )
    .await;

    let stored = p.sink.snapshot();
    assert_eq!(
        stored.len(),
        arrived.load(Ordering::Relaxed) as usize,
        "every check that reached the server is stored"
    );
    assert!(stored.iter().all(|r| r.status == CheckStatus::Up));
}

#[tokio::test]
async fn a_check_outlasting_the_settle_window_does_not_hold_the_flush() {
    let (addr, arrived) = spawn_slow_mock(Duration::from_secs(30)).await;
    let p = mid_check(addr, &arrived).await;

    let started = tokio::time::Instant::now();
    stop_probing(
        &p.root,
        p.scheduler,
        &p.pool,
        Duration::from_millis(200),
        p.flush,
        p.batcher,
    )
    .await;

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(p.sink.is_empty());
}

#[tokio::test]
async fn an_agent_stays_seen_while_its_checks_settle_and_stops_after() {
    use axum::http::HeaderMap;
    use axum::http::header::{ETAG, IF_NONE_MATCH};
    const VERSION: &str = "\"v1\"";

    let pulls = Arc::new(AtomicU32::new(0));
    let full_pulls = Arc::new(AtomicU32::new(0));
    let control_plane = {
        let (pulls, full_pulls) = (pulls.clone(), full_pulls.clone());
        spawn_router(Router::new().route(
            "/api/agent/targets",
            get(move |headers: HeaderMap| {
                let (pulls, full_pulls) = (pulls.clone(), full_pulls.clone());
                async move {
                    pulls.fetch_add(1, Ordering::Relaxed);
                    if headers.get(IF_NONE_MATCH).is_some_and(|v| v == VERSION) {
                        (StatusCode::NOT_MODIFIED, [(ETAG, VERSION)], "")
                    } else {
                        full_pulls.fetch_add(1, Ordering::Relaxed);
                        (StatusCode::OK, [(ETAG, VERSION)], "{}")
                    }
                }
            }),
        ))
        .await
    };
    let agent = uptimepage::agent::AgentPullSource::new(
        uptimepage::http_outbound::build_outbound_client(uptimepage::security::SsrfGuard::new(
            true,
        )),
        &format!("http://{control_plane}"),
        "token".into(),
        false,
    );
    let (addr, arrived) = spawn_slow_mock(Duration::from_millis(800)).await;
    let p = mid_check(addr, &arrived).await;
    let every = Duration::from_millis(100);

    agent
        .keep_seen_while(
            stop_probing(
                &p.root,
                p.scheduler,
                &p.pool,
                Duration::from_secs(5),
                p.flush,
                p.batcher,
            ),
            every,
        )
        .await;

    assert_eq!(
        p.sink.len(),
        arrived.load(Ordering::Relaxed) as usize,
        "the settled check is stored"
    );
    tokio::time::sleep(every).await;
    let during = pulls.load(Ordering::Relaxed);
    assert!(during >= 2, "pulled {during} times while the check settled");
    assert_eq!(
        full_pulls.load(Ordering::Relaxed),
        1,
        "a new config version is sent in full once"
    );
    tokio::time::sleep(every * 5).await;
    assert_eq!(pulls.load(Ordering::Relaxed), during);
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

    let pool = test_pool(tx);
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
    let scheduler = Arc::new(Scheduler::new(registry, test_pool(tx), scheduler_cfg(1)));
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

/// `/` accepts the request and never answers, like an origin whose workers
/// hang; `/ok` answers.
async fn spawn_hanging() -> std::net::SocketAddr {
    let app = Router::new()
        .route("/", get(std::future::pending::<&'static str>))
        .route("/ok", get(|| async { "ok" }));
    spawn_router(app).await
}

fn with_timeout(mut target: Target, timeout: Duration) -> Target {
    if let CheckSpec::Http(http) = &mut target.check {
        http.timeout = timeout;
    }
    target
}

#[tokio::test]
async fn worker_pool_breaker_opens_on_timeouts_and_repeats_the_cause() {
    let addr = spawn_hanging().await;
    let target = with_timeout(http_target(addr, "/", 100), Duration::from_millis(150));
    let store = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let registry = Arc::new(TargetRegistry::new(store));
    let (tx, mut rx) = mpsc::channel(64);

    let pool = test_pool(tx);
    let scheduler = Arc::new(Scheduler::new(registry, pool.clone(), scheduler_cfg(30)));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.run(shutdown.clone()));

    let mut timeouts = 0;
    let mut skipped = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline && skipped.is_none() {
        if let Ok(Some(r)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            if r.is_served_stale() {
                skipped = Some(r);
            } else if r.error.as_deref() == Some("no response") {
                timeouts += 1;
            }
        }
    }
    shutdown.cancel();
    handle.await.unwrap().unwrap();

    let skipped = skipped.expect("breaker never opened");
    assert!(timeouts >= 2, "expected ≥2 timeouts, got {timeouts}");
    assert_eq!(skipped.status, CheckStatus::Error);
    assert_eq!(
        humanize_check_error(skipped.error.as_deref().unwrap()),
        "connected, but no response (timed out)"
    );
    assert_eq!(pool.open_breakers(), 1);

    // A check-now that timed out again leaves the breaker, even with the
    // status line in (a 200 whose body hangs); one that got an answer resets it.
    let timed_out = CheckResult::error(skipped.target_id, skipped.org_id, "no response");
    pool.note_check_now(skipped.target_id, &timed_out);
    assert_eq!(pool.open_breakers(), 1);
    let body_hung = CheckResult {
        response_code: Some(200),
        error: Some("body timeout".into()),
        ..timed_out.clone()
    };
    pool.note_check_now(skipped.target_id, &body_hung);
    assert_eq!(pool.open_breakers(), 1);
    let answered = CheckResult {
        status: CheckStatus::Up,
        error: None,
        ..timed_out
    };
    pool.note_check_now(skipped.target_id, &answered);
    assert_eq!(pool.open_breakers(), 0);
}

#[tokio::test]
async fn a_hanging_monitor_never_skips_another_on_the_same_host() {
    let addr = spawn_hanging().await;
    let hanging = with_timeout(http_target(addr, "/", 100), Duration::from_millis(150));
    let healthy = http_target(addr, "/ok", 100);
    let (hanging_id, healthy_id) = (hanging.id, healthy.id);
    let store = Arc::new(InMemoryTargetStore::from_vec(vec![hanging, healthy]));
    let registry = Arc::new(TargetRegistry::new(store));
    let (tx, mut rx) = mpsc::channel(64);

    let pool = test_pool(tx);
    let scheduler = Arc::new(Scheduler::new(registry, pool.clone(), scheduler_cfg(30)));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.run(shutdown.clone()));

    let mut hanging_skipped = 0;
    let mut healthy_up = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline && hanging_skipped < 2 {
        if let Ok(Some(r)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            if r.target_id == healthy_id {
                assert_eq!(r.status, CheckStatus::Up, "healthy monitor: {:?}", r.error);
                healthy_up += 1;
            } else if r.target_id == hanging_id && r.is_served_stale() {
                hanging_skipped += 1;
            }
        }
    }
    shutdown.cancel();
    handle.await.unwrap().unwrap();

    assert!(
        hanging_skipped >= 2,
        "hanging monitor's breaker never opened"
    );
    assert!(
        healthy_up >= 2,
        "expected ≥2 healthy checks, got {healthy_up}"
    );
    assert_eq!(pool.open_breakers(), 1);
}

#[tokio::test]
async fn an_edited_monitor_probes_on_a_fresh_breaker() {
    let addr = spawn_hanging().await;
    let org = OrgId(uuid::Uuid::from_u128(1));
    let hanging = with_timeout(http_target(addr, "/", 100), Duration::from_millis(150));
    let source = Arc::new(MutableSource::new(vec![(org, hanging.clone())]));
    let registry = Arc::new(TargetRegistry::new(source.clone()));
    let (tx, mut rx) = mpsc::channel(64);

    let pool = test_pool(tx);
    let scheduler = Arc::new(Scheduler::new(registry, pool.clone(), scheduler_cfg(1)));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.run(shutdown.clone()));

    let opened = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(r) = rx.recv().await {
            if r.is_served_stale() {
                return;
            }
        }
    })
    .await;
    assert!(opened.is_ok(), "breaker never opened");

    // The breaker stays open for 30 s; a fix to the URL must not wait it out.
    let mut fixed = hanging.clone();
    if let CheckSpec::Http(http) = &mut fixed.check {
        http.url = format!("http://{addr}/ok").parse().unwrap();
    }
    fixed.updated_at = chrono::Utc::now();
    source.set(vec![(org, fixed)]);

    let up = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(r) = rx.recv().await {
            if r.status == CheckStatus::Up {
                return;
            }
        }
    })
    .await;
    shutdown.cancel();
    handle.await.unwrap().unwrap();

    assert!(up.is_ok(), "the edited monitor kept its open breaker");
    assert_eq!(pool.open_breakers(), 0);
}

#[tokio::test]
async fn worker_pool_breaker_stays_closed_on_a_host_that_answers() {
    let addr = spawn_always_500().await;
    let store = Arc::new(InMemoryTargetStore::from_vec(vec![http_target(
        addr, "/", 50,
    )]));
    let registry = Arc::new(TargetRegistry::new(store));
    let (tx, mut rx) = mpsc::channel(64);

    let pool = test_pool(tx);
    let scheduler = Arc::new(Scheduler::new(registry, pool.clone(), scheduler_cfg(30)));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.run(shutdown.clone()));

    let mut downs = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline && downs < 6 {
        if let Ok(Some(r)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            assert!(
                !r.is_served_stale(),
                "a 500 opened the breaker: {:?}",
                r.error
            );
            if r.status == CheckStatus::Down {
                downs += 1;
            }
        }
    }
    shutdown.cancel();
    handle.await.unwrap().unwrap();

    assert!(downs >= 6, "expected ≥6 downs, got {downs}");
    assert_eq!(pool.open_breakers(), 0);
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

    let pool = test_pool(tx);
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

const FLEET: usize = 200;

/// Monitors nobody has touched for a day, so none is due a quick first probe.
fn fleet(addr: std::net::SocketAddr) -> Vec<(OrgId, Target)> {
    let org = OrgId(uuid::Uuid::from_u128(1));
    (0..FLEET)
        .map(|_| {
            let mut target = http_target(addr, "/ping", 60_000);
            target.updated_at = chrono::Utc::now() - chrono::Duration::days(1);
            (org, target)
        })
        .collect()
}

#[tokio::test]
async fn monitors_loaded_at_start_are_not_all_probed_at_once() {
    let (addr, counter) = spawn_counting_mock().await;
    let source = Arc::new(MutableSource::new(fleet(addr)));
    let (tx, mut rx) = mpsc::channel(FLEET * 2);
    let scheduler = Arc::new(Scheduler::new(
        Arc::new(TargetRegistry::new(source)),
        test_pool(tx),
        scheduler_cfg(30),
    ));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.run(shutdown.clone()));

    tokio::time::sleep(Duration::from_secs(3)).await;
    let probed = counter.load(Ordering::Relaxed);
    shutdown.cancel();
    handle.await.unwrap().unwrap();
    rx.close();

    assert!(probed < 40, "{probed} of {FLEET} probed in the first 3 s");
}

#[tokio::test]
async fn a_fleet_that_arrives_after_an_empty_start_is_not_probed_at_once() {
    let (addr, counter) = spawn_counting_mock().await;
    let source = Arc::new(MutableSource::new(vec![]));
    let (tx, mut rx) = mpsc::channel(FLEET * 2);
    let scheduler = Arc::new(Scheduler::new(
        Arc::new(TargetRegistry::new(source.clone())),
        test_pool(tx),
        scheduler_cfg(1),
    ));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.run(shutdown.clone()));

    tokio::time::sleep(Duration::from_millis(200)).await;
    source.set(fleet(addr));
    // One refresh to pick the fleet up, then 3 s of probing.
    tokio::time::sleep(Duration::from_millis(4_100)).await;
    let probed = counter.load(Ordering::Relaxed);
    shutdown.cancel();
    handle.await.unwrap().unwrap();
    rx.close();

    assert!(
        probed < 40,
        "{probed} of {FLEET} probed within 4 s of arriving"
    );
}

#[tokio::test]
async fn a_monitor_added_while_running_is_probed_within_seconds() {
    let (addr, _counter) = spawn_counting_mock().await;
    let source = Arc::new(MutableSource::new(vec![]));
    let registry = Arc::new(TargetRegistry::new(source.clone()));
    let (tx, mut rx) = mpsc::channel(64);
    let scheduler = Arc::new(Scheduler::new(registry, test_pool(tx), scheduler_cfg(1)));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(scheduler.run(shutdown.clone()));

    tokio::time::sleep(Duration::from_millis(200)).await;
    let org = OrgId(uuid::Uuid::from_u128(1));
    source.set(vec![(org, http_target(addr, "/ping", 3_600_000))]);
    let first = tokio::time::timeout(Duration::from_secs(8), rx.recv()).await;

    shutdown.cancel();
    handle.await.unwrap().unwrap();
    assert!(matches!(first, Ok(Some(_))), "no probe for the new monitor");
}

/// Each connection gets the next response, then is closed.
async fn control_plane_answering(responses: Vec<Vec<u8>>) -> AgentPullSource {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for response in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut scratch = [0u8; 4096];
            let _ = sock.read(&mut scratch).await;
            let _ = sock.write_all(&response).await;
        }
    });
    AgentPullSource::new(
        uptimepage::http_outbound::build_control_plane_client(
            uptimepage::security::SsrfGuard::relaxed_for_tests(),
        ),
        &format!("http://{addr}"),
        "token".into(),
        false,
    )
}

fn pull_with_one_target() -> (Target, Vec<u8>) {
    let target = http_target("127.0.0.1:9".parse().unwrap(), "/", 1000);
    let body = serde_json::to_vec(&AgentTargetsResponse {
        region: "eu".into(),
        targets: vec![AgentTargetDto {
            org_id: uuid::Uuid::now_v7(),
            target: target.clone(),
        }],
    })
    .unwrap();
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend(body);
    (target, response)
}

#[tokio::test]
async fn agent_serves_last_known_config_when_a_pull_loses_its_connection_mid_body() {
    let (target, first_pull) = pull_with_one_target();
    let cut_off = b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{".to_vec();
    let pull = control_plane_answering(vec![first_pull, cut_off]).await;

    pull.list_all_enabled_targets().await.expect("first pull");

    let served = pull
        .list_all_enabled_targets()
        .await
        .expect("a lost connection is transient");
    let ids: Vec<_> = served.iter().map(|(_, t)| t.id).collect();
    assert_eq!(ids, [target.id]);
}

#[tokio::test]
async fn agent_reports_a_pull_over_the_size_limit_as_a_failure() {
    let (_, first_pull) = pull_with_one_target();
    let size = MAX_PULL_BYTES + 1;
    let mut oversized = format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\n\r\n").into_bytes();
    oversized.resize(oversized.len() + size, b' ');
    let pull = control_plane_answering(vec![first_pull, oversized]).await;

    pull.list_all_enabled_targets().await.expect("first pull");

    pull.list_all_enabled_targets()
        .await
        .expect_err("a config over the limit will not fix itself");
}
