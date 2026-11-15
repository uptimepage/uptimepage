//! Range reads answer from raw results inside the org's raw window and from
//! the hour rollup past it.
//!
//!     DATABASE_URL=… CLICKHOUSE_URL=http://127.0.0.1:8123 \
//!       cargo nextest run --test it --run-ignored all -E 'test(/^results_source_test::/)'

use crate::common;

use chrono::{DateTime, Duration, Timelike, Utc};
use sqlx::PgPool;
use uptimepage::domain::{CheckResult, CheckStatus, OrgId};
use uptimepage::storage::{
    ClampedRange, ClickhouseResultSink, ClickhouseResultsStore, OrgTtlDays, ResultSink,
    ResultsStore, TimeRange,
};
use uuid::Uuid;

fn up(target: Uuid, org: Uuid, ts: DateTime<Utc>) -> CheckResult {
    CheckResult {
        target_id: target,
        org_id: org,
        timestamp: ts,
        status: CheckStatus::Up,
        duration_ms: 100,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: None,
        response_size: None,
        network: None,
        diagnostic: None,
        error: None,
        details: None,
    }
}

fn top_of_hour(days_ago: i64) -> DateTime<Utc> {
    (Utc::now() - Duration::days(days_ago))
        .with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .expect("valid time")
}

/// An org on an unlisted copy of the free plan that keeps raw rows `raw_days`.
async fn org_keeping_raw_for(pool: &PgPool, raw_days: i32) -> (Uuid, String) {
    let plan = format!("raw-{raw_days}d-{}", Uuid::now_v7().simple());
    sqlx::query(
        "INSERT INTO plans \
         SELECT (jsonb_populate_record(NULL::plans, to_jsonb(p) || jsonb_build_object( \
             'id', $1::text, 'raw_days', $2::int, 'is_listed', false))).* \
         FROM plans p WHERE p.id = 'free'",
    )
    .bind(&plan)
    .bind(raw_days)
    .execute(pool)
    .await
    .expect("insert plan");
    let slug = common::unique_slug("raw-window");
    let (org,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts (plan_id) VALUES ($1) RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $2, 'Raw window', a.id FROM a RETURNING id",
    )
    .bind(&plan)
    .bind(&slug)
    .fetch_one(pool)
    .await
    .expect("insert org");
    (org, plan)
}

async fn drop_org_and_plan(pool: &PgPool, org: Uuid, plan: &str) {
    sqlx::query(
        "WITH o AS (DELETE FROM organizations WHERE id = $1 RETURNING account_id) \
         DELETE FROM accounts WHERE id IN (SELECT account_id FROM o)",
    )
    .bind(org)
    .execute(pool)
    .await
    .expect("delete org");
    sqlx::query("DELETE FROM plans WHERE id = $1")
        .bind(plan)
        .execute(pool)
        .await
        .expect("delete plan");
}

#[tokio::test]
#[ignore = "requires Postgres + ClickHouse (DATABASE_URL, CLICKHOUSE_URL)"]
async fn a_range_past_the_orgs_raw_window_reads_the_hour_rollup() {
    let (Some(pool), Some(ch)) = (
        common::pg_pool_from_env().await,
        common::ch_client_from_env().await,
    ) else {
        return;
    };
    let (org, plan) = org_keeping_raw_for(&pool, 7).await;
    let retention = OrgTtlDays::new();
    retention.refresh(&pool).await.expect("load retention");
    let sink = ClickhouseResultSink::new(
        ch.clone(),
        "default".into(),
        "default".into(),
        retention.clone(),
    );
    let store = ClickhouseResultsStore::from_client(ch.clone()).with_retention(retention);
    let target = Uuid::now_v7();
    let hour = top_of_hour(10);
    let rows: Vec<CheckResult> = (0..4)
        .map(|i| up(target, org, hour + Duration::minutes(i * 10)))
        .collect();
    sink.write_batch(&rows).await.expect("insert");
    // What the 7-day TTL does to these rows on its next merge.
    ch.query("DELETE FROM check_results WHERE org_id = ?")
        .bind(org)
        .execute()
        .await
        .expect("expire raw rows");

    let range = TimeRange {
        from: hour,
        to: hour + Duration::hours(1),
    };
    let stats = store
        .uptime(OrgId(org), target, ClampedRange::unclamped(range), None)
        .await
        .expect("uptime");
    drop_org_and_plan(&pool, org, &plan).await;
    assert_eq!((stats.total, stats.up), (4, 4));
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn a_recent_range_counts_raw_results_to_the_second() {
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let sink = ClickhouseResultSink::new(
        ch.clone(),
        "default".into(),
        "default".into(),
        OrgTtlDays::new(),
    );
    let store = ClickhouseResultsStore::from_client(ch);
    let org = Uuid::now_v7();
    let target = Uuid::now_v7();
    let hour = top_of_hour(1);
    let rows: Vec<CheckResult> = (0..4)
        .map(|i| up(target, org, hour + Duration::minutes(i * 10)))
        .collect();
    sink.write_batch(&rows).await.expect("insert");

    let range = TimeRange {
        from: hour + Duration::minutes(15),
        to: hour + Duration::minutes(35),
    };
    let stats = store
        .uptime(OrgId(org), target, ClampedRange::unclamped(range), None)
        .await
        .expect("uptime");
    assert_eq!(stats.total, 2, "only the :20 and :30 checks fall inside");
}
