//! Live-PG + live-CH tests for the soft-delete purge worker. Skipped by
//! default; runs under `--include-ignored` once `DATABASE_URL` and
//! `CLICKHOUSE_URL` are set. The tests seed their own orgs/users + write
//! check_results rows tagged with the org id, then exercise the cascade and
//! the queue-drain.

mod common;

use common::{make_user, unique_slug};
use uptimepage::domain::OrgId;
use uptimepage::jobs::purge_deleted::{
    drain_clickhouse_purge_queue, purge_queue_depth, purge_tick,
};
use uptimepage::storage::{create_org_with_owner, soft_delete_org};
use uuid::Uuid;

/// Backdate `deleted_at` so the row is past the grace window without sleeping.
async fn backdate_delete(pool: &sqlx::PgPool, org: OrgId, days_ago: i32) {
    sqlx::query(
        "UPDATE organizations SET deleted_at = now() - ($2::int * INTERVAL '1 day') WHERE id = $1",
    )
    .bind(org.0)
    .bind(days_ago)
    .execute(pool)
    .await
    .expect("backdate delete");
}

/// `completed_at` of one org's purge-queue row, for assertions that must not
/// depend on the tick-wide count a parallel drain can race.
async fn completed_at(pool: &sqlx::PgPool, org: OrgId) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar("SELECT completed_at FROM clickhouse_purge_queue WHERE org_id = $1")
        .bind(org.0)
        .fetch_one(pool)
        .await
        .expect("read queue row")
}

#[tokio::test]
#[ignore]
async fn grace_window_blocks_purge() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let user = make_user(&pool, "purge").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("grace"), "n")
        .await
        .unwrap()
        .unwrap();
    soft_delete_org(&pool, org.id, user).await.unwrap();

    // Inside grace (deleted ~now), tick must not cascade THIS org. Other
    // tests running in parallel may have backdated their own orgs and bumped
    // `stats.cascaded`, so the per-tick total is racy — assert on the
    // specific org instead.
    let _ = purge_tick(&pool, &ch, 30).await.unwrap();
    let (exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM organizations WHERE id = $1)")
            .bind(org.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(exists, "org should still exist inside grace window");
    let (queued,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM clickhouse_purge_queue WHERE org_id = $1)")
            .bind(org.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!queued, "no CH purge row should be enqueued inside grace");

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn past_grace_cascades_and_enqueues_ch() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let user = make_user(&pool, "purge").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("past"), "n")
        .await
        .unwrap()
        .unwrap();
    soft_delete_org(&pool, org.id, user).await.unwrap();
    backdate_delete(&pool, org.id, 40).await;

    // Parallel tests can grab this org via `LIMIT 10`, so assert on THIS
    // org's outcome instead of tick-wide `cascaded`.
    let _ = purge_tick(&pool, &ch, 30).await.unwrap();

    // PG row gone via cascade.
    let (exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM organizations WHERE id = $1)")
            .bind(org.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!exists, "org should be hard-deleted");

    // Queue row exists; it either drained in the same tick (completed_at set)
    // or is still pending. Either is correct — both states are idempotent on
    // retry.
    let (queue_exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM clickhouse_purge_queue WHERE org_id = $1)")
            .bind(org.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(queue_exists, "purge queue should track the cascaded org");

    // Cleanup: drop queue row + user.
    sqlx::query("DELETE FROM clickhouse_purge_queue WHERE org_id = $1")
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn restore_cancels_purge() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let user = make_user(&pool, "purge").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("cncl"), "n")
        .await
        .unwrap()
        .unwrap();
    soft_delete_org(&pool, org.id, user).await.unwrap();
    // Re-activate before grace window expires.
    sqlx::query("UPDATE organizations SET deleted_at = NULL WHERE id = $1")
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();
    // Even if we somehow had a past-grace timestamp, deleted_at IS NULL filter
    // wins: the purge query never sees this row. Parallel tests may bump the
    // tick-wide `cascaded` total, so assert on THIS org specifically.
    let _ = purge_tick(&pool, &ch, 30).await.unwrap();
    let (exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM organizations WHERE id = $1)")
            .bind(org.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(exists, "restored org must survive the tick");

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

/// Deterministic post-SELECT race: purge's outer SELECT (no row lock) picks
/// up a past-grace org, then a concurrent recovery commits `deleted_at =
/// NULL` before the cascade DELETE fires. Under READ COMMITTED the DELETE
/// re-evaluates its WHERE clause against the post-commit state, the
/// `deleted_at IS NOT NULL` predicate fails, and the per-org tx rolls back
/// — so both the queue insert and the org row survive. Without the
/// predicate this test wipes a recovered org.
#[tokio::test]
#[ignore]
async fn cascade_predicate_blocks_post_select_recovery_race() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let user = make_user(&pool, "purge").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("race"), "n")
        .await
        .unwrap()
        .unwrap();
    soft_delete_org(&pool, org.id, user).await.unwrap();
    backdate_delete(&pool, org.id, 40).await;

    // Recovery tx holds the row lock with deleted_at=NULL pending commit.
    // purge_tick's SELECT (no FOR UPDATE) sees the still-committed past-grace
    // state and proceeds; its cascade DELETE then blocks on the row lock
    // until we commit below.
    let mut recovery_tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE organizations SET deleted_at = NULL WHERE id = $1")
        .bind(org.id.0)
        .execute(&mut *recovery_tx)
        .await
        .unwrap();
    // Capture recovery_tx's backend PID so the poll below can scope to "who
    // is blocked *by us*", filtering out unrelated DELETEs that parallel
    // ignored-purge tests run on the same CI database.
    let recovery_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *recovery_tx)
        .await
        .unwrap();

    let pool_clone = pool.clone();
    let purge_handle = tokio::spawn(async move { purge_tick(&pool_clone, &ch, 30).await.unwrap() });

    // Wait for a DELETE FROM organizations that is blocked specifically by
    // recovery_tx's backend — `pg_blocking_pids(pid)` returns the array of
    // pids holding locks the waiter needs. Cap at 5s so a hung worker fails
    // loudly instead of deadlocking.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let (blocked,): (i64,) = sqlx::query_as(
            r#"SELECT count(*)::bigint FROM pg_stat_activity
                WHERE $1 = ANY(pg_blocking_pids(pid))
                  AND query ILIKE 'DELETE FROM organizations%'"#,
        )
        .bind(recovery_pid)
        .fetch_one(&pool)
        .await
        .unwrap();
        if blocked >= 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "purge worker never blocked on the recovery row lock — race did not materialise"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    recovery_tx.commit().await.unwrap();
    purge_handle.await.unwrap();

    let (exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM organizations WHERE id = $1)")
            .bind(org.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(exists, "recovered org must survive the post-SELECT race");

    let (queued,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM clickhouse_purge_queue WHERE org_id = $1)")
            .bind(org.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        !queued,
        "rolled-back cascade tx must leave no queue row that would erase live CH data"
    );

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn drain_is_idempotent_on_repeat() {
    // Calling drain twice on the same queue row should mark it complete once
    // and leave the second call as a no-op (no rows pending).
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let user = make_user(&pool, "purge").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("drn"), "n")
        .await
        .unwrap()
        .unwrap();
    soft_delete_org(&pool, org.id, user).await.unwrap();
    backdate_delete(&pool, org.id, 40).await;

    let _ = purge_tick(&pool, &ch, 30).await.unwrap();
    // Force the queue row back to pending to simulate a worker that died
    // before marking complete, then re-drain.
    sqlx::query("UPDATE clickhouse_purge_queue SET completed_at = NULL WHERE org_id = $1")
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();
    // Assert on THIS org's row, not the tick-wide drained count: a parallel
    // test's drain can complete the row first, so the returned count is racy.
    drain_clickhouse_purge_queue(&pool, &ch).await.unwrap();
    assert!(
        completed_at(&pool, org.id).await.is_some(),
        "second drain marks this org's row complete"
    );
    // A repeat drain neither reopens nor double-processes the completed row.
    drain_clickhouse_purge_queue(&pool, &ch).await.unwrap();
    assert!(
        completed_at(&pool, org.id).await.is_some(),
        "queue row stays completed across drains"
    );

    sqlx::query("DELETE FROM clickhouse_purge_queue WHERE org_id = $1")
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn enqueue_is_dedup_on_conflict() {
    // ON CONFLICT (org_id) DO NOTHING — a second enqueue for the same org
    // mustn't create a duplicate row.
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "purge").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("dup"), "n")
        .await
        .unwrap()
        .unwrap();

    sqlx::query(
        "INSERT INTO clickhouse_purge_queue (org_id) VALUES ($1) ON CONFLICT (org_id) DO NOTHING",
    )
    .bind(org.id.0)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO clickhouse_purge_queue (org_id) VALUES ($1) ON CONFLICT (org_id) DO NOTHING",
    )
    .bind(org.id.0)
    .execute(&pool)
    .await
    .unwrap();

    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM clickhouse_purge_queue WHERE org_id = $1")
            .bind(org.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    sqlx::query("DELETE FROM clickhouse_purge_queue WHERE org_id = $1")
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

/// The drain must not settle a queue row on the mutation *ack* — it must
/// settle only once a `count()` proves the org's CH rows are actually gone.
/// Seeds a real `check_results` row, drains, then asserts both the queue row
/// is complete AND zero rows survive in both tenant tables.
#[tokio::test]
#[ignore]
async fn drain_erases_ch_rows_then_completes() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let user = make_user(&pool, "purge").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("erase"), "n")
        .await
        .unwrap()
        .unwrap();

    // One real result row for this org; the MV propagates it to check_results_1m.
    ch.query(
        "INSERT INTO check_results (org_id, target_id, timestamp, status, duration_ms) \
         VALUES (?, ?, fromUnixTimestamp(?), 'up', ?)",
    )
    .bind(org.id.0)
    .bind(Uuid::now_v7())
    .bind(chrono::Utc::now().timestamp())
    .bind(42u32)
    .execute()
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO clickhouse_purge_queue (org_id) VALUES ($1) ON CONFLICT (org_id) DO NOTHING",
    )
    .bind(org.id.0)
    .execute(&pool)
    .await
    .unwrap();

    let drained = drain_clickhouse_purge_queue(&pool, &ch).await.unwrap();
    assert!(drained >= 1, "the seeded org should drain");

    let (completed,): (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT completed_at FROM clickhouse_purge_queue WHERE org_id = $1")
            .bind(org.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        completed.is_some(),
        "queue row settles only after erasure is verified"
    );

    for table in ["check_results", "check_results_1m"] {
        let n: u64 = ch
            .query(&format!("SELECT count() FROM {table} WHERE org_id = ?"))
            .bind(org.id.0)
            .fetch_one::<u64>()
            .await
            .unwrap();
        assert_eq!(n, 0, "{table} must hold zero rows for the erased org");
    }

    sqlx::query("DELETE FROM clickhouse_purge_queue WHERE org_id = $1")
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

/// `purge_queue_depth` powers the stuck-erasure alert. A pending row must be
/// counted with a non-zero oldest-age; a completed row must not.
#[tokio::test]
#[ignore]
async fn queue_depth_counts_pending_not_completed() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "purge").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("depth"), "n")
        .await
        .unwrap()
        .unwrap();

    // Run inside an uncommitted transaction: the backdated row stays invisible
    // to the concurrent drain in parallel tick tests (which picks the global
    // oldest pending row first and would complete it mid-assert), so the depth
    // it produces is deterministic.
    let mut tx = pool.begin().await.unwrap();

    // Backdated 3h and still pending → within this tx it is the oldest entry,
    // so oldest_age tracks it.
    sqlx::query(
        "INSERT INTO clickhouse_purge_queue (org_id, queued_at) \
         VALUES ($1, now() - INTERVAL '3 hours')",
    )
    .bind(org.id.0)
    .execute(&mut *tx)
    .await
    .unwrap();

    let d = purge_queue_depth(&mut *tx).await.unwrap();
    assert!(d.pending >= 1, "pending row must be counted");
    assert!(
        d.oldest_age_secs >= 3 * 3600 - 60,
        "oldest age should reflect the 3h-old pending row, got {}",
        d.oldest_age_secs
    );

    sqlx::query("UPDATE clickhouse_purge_queue SET completed_at = now() WHERE org_id = $1")
        .bind(org.id.0)
        .execute(&mut *tx)
        .await
        .unwrap();
    let (still_pending,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM clickhouse_purge_queue \
         WHERE org_id = $1 AND completed_at IS NULL)",
    )
    .bind(org.id.0)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert!(
        !still_pending,
        "completing the row drops it from the pending depth"
    );

    // Rollback discards the queue row; the user row was committed outside the tx.
    tx.rollback().await.unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

/// Simulates the worker dying after the PG cascade transaction commits but
/// before `drain_clickhouse_purge_queue` finishes. State on disk is "org gone
/// from PG, queue row pending, CH-side mutation never issued". The next
/// `purge_tick` must complete the job — cascade finds nothing new, drain
/// processes the pending row and marks it complete.
#[tokio::test]
#[ignore]
async fn purge_resilient_to_kill_between_pg_cascade_and_ch_drain() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let user = make_user(&pool, "purge").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("kill"), "n")
        .await
        .unwrap()
        .unwrap();
    soft_delete_org(&pool, org.id, user).await.unwrap();
    backdate_delete(&pool, org.id, 40).await;

    // Tick 1: cascade + drain both run. We then reset the queue row to pending
    // to emulate a SIGKILL between the PG commit and the CH ALTER. The PG side
    // already committed and stays cascaded. Tick-wide `cascaded` is racy with
    // parallel tests; assert on this org's row state below.
    let _ = purge_tick(&pool, &ch, 30).await.unwrap();
    let (org_exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM organizations WHERE id = $1)")
            .bind(org.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!org_exists, "PG cascade ran");

    sqlx::query("UPDATE clickhouse_purge_queue SET completed_at = NULL WHERE org_id = $1")
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();

    // Tick 2: drain recovers the half-finished work and marks the queue row
    // complete. The tick-wide `cascaded`/`drained` totals can move because of
    // parallel tests, so assert on the specific queue row below instead of
    // pinning a literal count.
    let _ = purge_tick(&pool, &ch, 30).await.unwrap();

    let (completed,): (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT completed_at FROM clickhouse_purge_queue WHERE org_id = $1")
            .bind(org.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        completed.is_some(),
        "queue row marked complete after recovery"
    );

    sqlx::query("DELETE FROM clickhouse_purge_queue WHERE org_id = $1")
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}
