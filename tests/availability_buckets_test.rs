//! ClickHouse integration test for the per-monitor availability buckets that
//! drive the uptime-card sparkline. Validates what in-memory unit tests can't:
//! the `countMerge`/`countIfMerge` deserialize path off the rollup and the
//! region predicate.
//!
//!     CLICKHOUSE_URL=http://127.0.0.1:8123 \
//!       cargo nextest run --test it --run-ignored only --no-capture -E 'test(/^availability_buckets_test::/)'

use crate::common;

use chrono::{Duration, Timelike, Utc};
use uptimepage::domain::{CheckResult, CheckStatus, OrgId};
use uptimepage::storage::{
    ClampedRange, ClickhouseResultSink, ClickhouseResultsStore, ResultSink, ResultsStore, TimeRange,
};
use uuid::Uuid;

fn check(target: Uuid, org: Uuid, ts: chrono::DateTime<Utc>, status: CheckStatus) -> CheckResult {
    CheckResult {
        target_id: target,
        org_id: org,
        timestamp: ts,
        status,
        duration_ms: 100,
        dns_ms: Some(10),
        connect_ms: Some(20),
        tls_ms: Some(30),
        ttfb_ms: Some(40),
        response_code: Some(200),
        response_size: Some(1024),
        diagnostic: None,
        error: None,
    }
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn availability_buckets_count_up_and_total_from_rollup() {
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let sink = ClickhouseResultSink::new(
        ch.clone(),
        "default".into(),
        "default".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );
    let store = ClickhouseResultsStore::from_client(ch);
    let org = Uuid::now_v7();
    let target = Uuid::now_v7();
    let base = (Utc::now() - Duration::hours(2))
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    // One minute: 3 up + 1 down + 1 degraded → total 5, up 3.
    sink.write_batch(&[
        check(target, org, base, CheckStatus::Up),
        check(target, org, base + Duration::seconds(10), CheckStatus::Up),
        check(target, org, base + Duration::seconds(20), CheckStatus::Up),
        check(target, org, base + Duration::seconds(30), CheckStatus::Down),
        check(
            target,
            org,
            base + Duration::seconds(40),
            CheckStatus::Degraded,
        ),
    ])
    .await
    .expect("insert samples");

    let range = TimeRange {
        from: base - Duration::minutes(1),
        to: base + Duration::minutes(5),
    };
    let buckets = store
        .availability_buckets(OrgId(org), target, ClampedRange::unclamped(range), 60, None)
        .await
        .expect("availability_buckets query");

    assert_eq!(buckets.len(), 1, "all samples land in one 60s bucket");
    assert_eq!(buckets[0].bucket_ts, base.timestamp());
    assert_eq!(buckets[0].total, 5);
    assert_eq!(buckets[0].up, 3, "countIfMerge counts only status=up");

    // Region predicate: matching region returns the bucket, others are empty.
    let same = store
        .availability_buckets(
            OrgId(org),
            target,
            ClampedRange::unclamped(range),
            60,
            Some("default"),
        )
        .await
        .expect("region-filtered query");
    assert_eq!(same.len(), 1);
    let other = store
        .availability_buckets(
            OrgId(org),
            target,
            ClampedRange::unclamped(range),
            60,
            Some("eu-west"),
        )
        .await
        .expect("other-region query");
    assert!(other.is_empty(), "no rows for an unused region");

    // uptime() merges the same rollup counts over the window.
    let up = store
        .uptime(OrgId(org), target, ClampedRange::unclamped(range), None)
        .await
        .expect("uptime query");
    assert_eq!(
        (up.total, up.up, up.down, up.degraded, up.error),
        (5, 3, 1, 1, 0)
    );
    assert!(
        up.uptime_pct.is_some_and(|p| (p - 60.0).abs() < 0.001),
        "3/5 = 60%"
    );

    // An empty window reads as unknown uptime, not 0%.
    let empty = TimeRange {
        from: base - Duration::hours(3),
        to: base - Duration::hours(2),
    };
    let none = store
        .uptime(OrgId(org), target, ClampedRange::unclamped(empty), None)
        .await
        .expect("empty-window uptime query");
    assert_eq!(none.total, 0);
    assert_eq!(none.uptime_pct, None, "no observations is not 0% uptime");
}
