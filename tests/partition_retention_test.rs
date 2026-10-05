//! Live-PG tests for month-partitioned audit tables: inserts route to a
//! concrete partition (not the DEFAULT backstop), ids are time-ordered v7, and
//! aged-out partitions are dropped while the current month survives.
//! Skipped by default; run under `--run-ignored` with `DATABASE_URL` set.

use crate::common;

use uptimepage::storage::partitions::{self, PARTITIONED_TABLES};

use crate::common::MIGRATOR;

async fn scalar<T>(pool: &sqlx::PgPool, sql: &str) -> T
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
{
    let (v,): (T,) = sqlx::query_as(sql).fetch_one(pool).await.expect("scalar");
    v
}

/// `default_partition_rows` is 0 when every write routes to a concrete month,
/// and counts rows that landed in DEFAULT (a month outside the provisioned
/// window — the stranding the gauge exists to alert on). Isolated DB so the
/// global `login_attempts` count is deterministic.
#[tokio::test]
#[ignore]
async fn default_partition_rows_reflects_stranded_rows() {
    let Some((db_url, db_name)) = common::fresh_test_db("partmaint").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();
    partitions::ensure_partitions(&pool).await.unwrap();

    assert_eq!(
        partitions::default_partition_rows(&pool, "login_attempts")
            .await
            .unwrap(),
        0,
        "nothing stranded yet"
    );
    // A month far outside [-1, +3] has no concrete partition → lands in DEFAULT.
    sqlx::query(
        "INSERT INTO login_attempts (method, success, occurred_at) \
         VALUES ('t', true, now() - INTERVAL '400 days')",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        partitions::default_partition_rows(&pool, "login_attempts")
            .await
            .unwrap(),
        1
    );
    // A current-month write routes to a concrete partition, not DEFAULT.
    sqlx::query("INSERT INTO login_attempts (method, success) VALUES ('t', true)")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        partitions::default_partition_rows(&pool, "login_attempts")
            .await
            .unwrap(),
        1,
        "current-month row must not land in DEFAULT"
    );

    pool.close().await;
    common::drop_test_db(&db_name).await;
}

/// `ensure_partitions` is safe to call repeatedly (boot + every daily tick):
/// the second run creates nothing new and never errors.
#[tokio::test]
#[ignore]
async fn ensure_partitions_is_idempotent() {
    let Some((db_url, db_name)) = common::fresh_test_db("partmaint").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();

    partitions::ensure_partitions(&pool).await.unwrap();
    let n1: i64 = scalar(
        &pool,
        "SELECT count(*) FROM pg_inherits WHERE inhparent = 'login_attempts'::regclass",
    )
    .await;
    partitions::ensure_partitions(&pool).await.unwrap();
    let n2: i64 = scalar(
        &pool,
        "SELECT count(*) FROM pg_inherits WHERE inhparent = 'login_attempts'::regclass",
    )
    .await;
    assert_eq!(n1, n2, "second ensure must not add partitions");
    assert!(n1 >= 2, "DEFAULT + provisioned months present");

    pool.close().await;
    common::drop_test_db(&db_name).await;
}

#[tokio::test]
#[ignore]
async fn provisions_partitions_routes_inserts_and_drops_aged() {
    let Some((db_url, db_name)) = common::fresh_test_db("partmaint").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();

    partitions::ensure_partitions(&pool).await.expect("ensure");

    for table in PARTITIONED_TABLES {
        // Parent + DEFAULT + provisioned months → several concrete children.
        let children: i64 = scalar(
            &pool,
            &format!("SELECT count(*) FROM pg_inherits WHERE inhparent = '{table}'::regclass"),
        )
        .await;
        assert!(
            children >= 2,
            "{table} should have month partitions, got {children}"
        );
    }

    // A fresh login_attempt lands in the current-month partition, never DEFAULT.
    let part: String = scalar(
        &pool,
        "WITH ins AS (
             INSERT INTO login_attempts (method, success) VALUES ('parttest', true)
             RETURNING tableoid, id
         )
         SELECT ins.tableoid::regclass::text || '|' || ins.id::text FROM ins",
    )
    .await;
    let (partition, id) = part.split_once('|').unwrap();
    assert!(
        partition.starts_with("login_attempts_p"),
        "insert routed to {partition}, expected a concrete month partition"
    );
    // RFC 9562 v7: the 13th hex digit (version nibble) is 7.
    assert_eq!(
        &id.chars().nth(14).unwrap().to_string(),
        "7",
        "id {id} is not a v7 uuid"
    );

    // Drop everything strictly before the current month: past partitions go,
    // the current month (and its just-inserted row) stays.
    let before: i64 = scalar(
        &pool,
        "SELECT count(*) FROM login_attempts WHERE method = 'parttest'",
    )
    .await;
    assert_eq!(before, 1);
    partitions::drop_old_partitions(&pool, "login_attempts", 0)
        .await
        .expect("drop");
    let after: i64 = scalar(
        &pool,
        "SELECT count(*) FROM login_attempts WHERE method = 'parttest'",
    )
    .await;
    assert_eq!(
        after, 1,
        "current-month row must survive a drop of aged partitions"
    );

    pool.close().await;
    common::drop_test_db(&db_name).await;
}
