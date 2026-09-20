mod common;

use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uptimepage::scheduler::TargetRegistry;
use uptimepage::scheduler::sampler;
use uptimepage::storage::InMemoryTargetStore;
use uptimepage::worker::{ResultFanout, WorkerPool};

use crate::common::{breaker_cfg, http_target, test_client};

#[tokio::test]
async fn sampler_runs_and_shuts_down() {
    // Long-interval target so its loop never fires during the test; we only
    // need a non-empty registry to exercise targets_total.
    let addr = "127.0.0.1:1".parse().unwrap();
    let store = Arc::new(InMemoryTargetStore::from_vec(vec![http_target(
        addr, "/", 60_000,
    )]));
    let registry = Arc::new(TargetRegistry::new(store.clone()));
    registry.refresh().await.unwrap();

    let (tx, _rx) = mpsc::channel(128);
    let pool = Arc::new(WorkerPool::new(
        16,
        test_client(),
        breaker_cfg(),
        ResultFanout::new(tx.clone()),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        common::test_domain_expiry_runtime(),
    ));

    assert_eq!(pool.max_concurrent(), 16);
    assert_eq!(pool.in_flight(), 0);
    assert_eq!(registry.len(), 1);
    assert_eq!(tx.max_capacity(), 128);

    // Lazy connect — never actually opens a Postgres connection; pool stats
    // (size, num_idle) both report 0 and the gauges sample cleanly without
    // requiring a real DB for this smoke test.
    let pg_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://localhost:1/none")
        .expect("lazy pool");

    // Dead CH endpoint — the parts-count sample fails each tick and is skipped
    // (best-effort), which is exactly the resilience this asserts.
    let ch = clickhouse::Client::default().with_url("http://127.0.0.1:1");

    let shutdown = CancellationToken::new();
    let h = sampler::spawn(
        pool,
        registry,
        Some(sampler::DbSources { pg_pool, ch }),
        &tx,
        Duration::from_millis(20),
        shutdown.clone(),
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(1), h)
        .await
        .expect("sampler did not shut down")
        .unwrap();
}

// The probe-only agent runs the sampler with no database; it must sample the
// worker/queue gauges and shut down cleanly without a Postgres or CH handle.
#[tokio::test]
async fn sampler_runs_without_database() {
    let addr = "127.0.0.1:1".parse().unwrap();
    let store = Arc::new(InMemoryTargetStore::from_vec(vec![http_target(
        addr, "/", 60_000,
    )]));
    let registry = Arc::new(TargetRegistry::new(store.clone()));
    registry.refresh().await.unwrap();

    let (tx, _rx) = mpsc::channel(128);
    let pool = Arc::new(WorkerPool::new(
        16,
        test_client(),
        breaker_cfg(),
        ResultFanout::new(tx.clone()),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        common::test_domain_expiry_runtime(),
    ));

    let shutdown = CancellationToken::new();
    let h = sampler::spawn(
        pool,
        registry,
        None,
        &tx,
        Duration::from_millis(20),
        shutdown.clone(),
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(1), h)
        .await
        .expect("sampler did not shut down")
        .unwrap();
}
