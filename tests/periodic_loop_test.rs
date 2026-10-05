//! Live-PG tests for the shared purge-loop ticker: the loop takes its advisory
//! lock through a real pool, so these need `DATABASE_URL` and
//! `--include-ignored`.

use crate::common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use uptimepage::jobs::periodic::{run_purge_loop, run_purge_loop_from_boot};

/// The lock is keyed by job name, so each test needs its own.
fn unique_job() -> &'static str {
    Box::leak(format!("test_loop_{}", uuid::Uuid::now_v7().simple()).into_boxed_str())
}

/// A loop that parks on an await the token cannot reach never returns; bound
/// every wait so that fails instead of hanging the suite.
const GRACE: Duration = Duration::from_secs(5);

/// Waits for `runs` to reach `want` rather than budgeting a fixed sleep for
/// three Postgres round trips, which a loaded runner will overrun.
async fn wait_for(runs: &AtomicUsize, want: usize) -> bool {
    tokio::time::timeout(GRACE, async {
        while runs.load(Ordering::SeqCst) < want {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

async fn stop(shutdown: CancellationToken, handle: tokio::task::JoinHandle<()>) {
    shutdown.cancel();
    tokio::time::timeout(GRACE, handle)
        .await
        .expect("the loop must return once shutdown is cancelled")
        .expect("loop task");
}

#[tokio::test]
#[ignore]
async fn the_boot_cadence_runs_before_its_first_period_is_up() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let runs = Arc::new(AtomicUsize::new(0));
    let shutdown = CancellationToken::new();

    let handle = {
        let (runs, pool, shutdown) = (Arc::clone(&runs), pool.clone(), shutdown.clone());
        tokio::spawn(run_purge_loop_from_boot(
            pool,
            shutdown,
            Duration::from_secs(60 * 60),
            unique_job(),
            async move |_: &sqlx::PgPool| {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok::<u64, String>(0)
            },
        ))
    };

    let ran = wait_for(&runs, 1).await;
    stop(shutdown, handle).await;
    assert!(
        ran,
        "a boot-cadence job must not wait out its hour-long period"
    );
}

#[tokio::test]
#[ignore]
async fn the_cleanup_cadence_owes_its_first_run_a_full_period() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let period = Duration::from_millis(400);
    let runs = Arc::new(AtomicUsize::new(0));
    let shutdown = CancellationToken::new();

    let handle = {
        let (runs, pool, shutdown) = (Arc::clone(&runs), pool.clone(), shutdown.clone());
        tokio::spawn(run_purge_loop(
            pool,
            shutdown,
            period,
            unique_job(),
            async move |_: &sqlx::PgPool| {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok::<u64, String>(0)
            },
        ))
    };

    tokio::time::sleep(period / 2).await;
    let at_half_a_period = runs.load(Ordering::SeqCst);
    let ran = wait_for(&runs, 1).await;
    stop(shutdown, handle).await;

    assert_eq!(
        at_half_a_period, 0,
        "boot must stay free of the cleanup jobs"
    );
    assert!(ran, "the loop must still tick once its period is up");
}
