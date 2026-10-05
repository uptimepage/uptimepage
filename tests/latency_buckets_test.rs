//! ClickHouse integration test for the bucketed latency series (issue #29).
//!
//! Validates the parts in-memory unit tests can't: the migration's new
//! per-phase `avgState` columns, the `quantilesMerge → Vec<f64>` deserialize
//! path, and the →0 fold for phases that are all-NULL (non-HTTP checks).
//!
//! Skipped by default — requires a ClickHouse the migrations have run against:
//!
//!     CLICKHOUSE_URL=http://127.0.0.1:8123 \
//!       cargo nextest run --test it --run-ignored only --no-capture -E 'test(/^latency_buckets_test::/)'

use crate::common;

use chrono::{Duration, Timelike, Utc};
use uptimepage::domain::{CheckResult, CheckStatus, OrgId};
use uptimepage::storage::{
    ClampedRange, ClickhouseResultSink, ClickhouseResultsStore, ResultSink, ResultsStore, TimeRange,
};
use uuid::Uuid;

fn http_ok(target: Uuid, org: Uuid, ts: chrono::DateTime<Utc>, dur: u32) -> CheckResult {
    CheckResult {
        target_id: target,
        org_id: org,
        timestamp: ts,
        status: CheckStatus::Up,
        duration_ms: dur,
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

fn tcp_ok(target: Uuid, org: Uuid, ts: chrono::DateTime<Utc>, dur: u32) -> CheckResult {
    CheckResult {
        target_id: target,
        org_id: org,
        timestamp: ts,
        status: CheckStatus::Up,
        duration_ms: dur,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: None,
        response_size: None,
        diagnostic: None,
        error: None,
    }
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn latency_buckets_merge_quantiles_and_phases_from_rollup() {
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
    // Six HTTP samples within one minute → one 60s bucket, varied durations
    // so the quantile spread is observable.
    let rows: Vec<CheckResult> = (0..6)
        .map(|i| {
            http_ok(
                target,
                org,
                base + Duration::seconds(i * 5),
                100 + (i as u32) * 50,
            )
        })
        .collect();
    sink.write_batch(&rows).await.expect("insert http samples");

    let range = TimeRange {
        from: base - Duration::minutes(1),
        to: base + Duration::minutes(5),
    };
    let buckets = store
        .latency_buckets(OrgId(org), target, ClampedRange::unclamped(range), 60, None)
        .await
        .expect("latency_buckets query");

    assert_eq!(buckets.len(), 1, "all samples land in one 60s bucket");
    let b = &buckets[0];
    assert_eq!(b.samples, 6);
    assert!(
        b.p50 > 0 && b.p95 >= b.p50 && b.p99 >= b.p95,
        "quantiles must be positive and monotonic: {b:?}"
    );
    assert_eq!(b.dns, 10, "mean dns phase");
    assert_eq!(b.connect, 20, "mean connect phase");
    assert_eq!(b.tls, 30, "mean tls phase");
    assert_eq!(b.ttfb, 40, "mean ttfb phase");
    assert!(b.avg >= 100, "mean total duration");
    assert_eq!(b.t, base.timestamp() * 1000, "bucket start in unix-millis");
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn latency_buckets_clamp_all_null_phases_to_zero() {
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
    let base = (Utc::now() - Duration::hours(3))
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    // TCP checks record no phase timings — every phase is NULL, so the
    // matview's avgMerge finalises to NULL. The query's ifNull must fold it
    // to 0 (NULL/NaN would not serialise as valid JSON).
    sink.write_batch(&[tcp_ok(target, org, base, 70)])
        .await
        .expect("insert tcp sample");

    let range = TimeRange {
        from: base - Duration::minutes(1),
        to: base + Duration::minutes(1),
    };
    let buckets = store
        .latency_buckets(OrgId(org), target, ClampedRange::unclamped(range), 60, None)
        .await
        .expect("latency_buckets query");

    assert_eq!(buckets.len(), 1);
    let b = &buckets[0];
    assert_eq!((b.dns, b.connect, b.tls, b.ttfb), (0, 0, 0, 0));
    assert_eq!(b.avg, 70);
    assert_eq!(b.p50, 70);
}

// A range reaching past the 1m rollup's 90-day TTL must route to check_results_1h.
#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn latency_buckets_route_to_hour_rollup_past_90d() {
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
    // 100 days ago, aligned to the hour. The matview aggregates on insert
    // (now), bucketing by the row's old timestamp into the 1h rollup.
    let base = (Utc::now() - Duration::days(100))
        .with_minute(0)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    let rows: Vec<CheckResult> = (0..4)
        .map(|i| {
            http_ok(
                target,
                org,
                base + Duration::minutes(i * 10),
                100 + (i as u32) * 50,
            )
        })
        .collect();
    sink.write_batch(&rows).await.expect("insert old samples");

    let range = TimeRange {
        from: base - Duration::hours(1),
        to: base + Duration::hours(1),
    };
    let buckets = store
        .latency_buckets(
            OrgId(org),
            target,
            ClampedRange::unclamped(range),
            3600,
            None,
        )
        .await
        .expect("latency_buckets query (1h path)");
    assert_eq!(buckets.len(), 1, "four samples in one hour → one 1h bucket");
    assert_eq!(buckets[0].samples, 4);
}
