//! ClickHouse-backed test for the per-region health sweep. Exercises the real
//! bind/deserialize path and the GROUP BY region aggregation.
//!
//! Skipped by default: requires the dev compose stack.
//!
//!     docker compose -f compose.dev.yml up -d
//!     DATABASE_URL=postgres://monitor:monitor@127.0.0.1:5432/monitor \
//!     CLICKHOUSE_URL=http://127.0.0.1:8123 \
//!       cargo nextest run --test it --run-ignored only -E 'test(/^region_health_test::/)'

use crate::common;

use chrono::Utc;
use uptimepage::domain::{CheckResult, CheckStatus};
use uptimepage::observability::region_health;
use uptimepage::storage::{ClickhouseResultSink, OrgTtlDays, ResultSink};
use uuid::Uuid;

fn result(target_id: Uuid, org_id: Uuid, status: CheckStatus, duration_ms: u32) -> CheckResult {
    CheckResult {
        target_id,
        org_id,
        timestamp: Utc::now(),
        status,
        duration_ms,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: Some(200),
        response_size: None,
        diagnostic: None,
        error: None,
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL — run via dev compose then `cargo test -- --ignored`"]
async fn collect_aggregates_per_region() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };

    // Unique regions per run so concurrent suites / leftover rows don't bleed in.
    let tag = Uuid::now_v7().simple().to_string();
    let region_a = format!("ra-{}", &tag[..8]);
    let region_b = format!("rb-{}", &tag[..8]);
    let org = Uuid::now_v7();
    let target = Uuid::now_v7();

    let sink_a = ClickhouseResultSink::new(
        ch.clone(),
        region_a.clone(),
        "agent-a".into(),
        OrgTtlDays::new(),
    );
    let sink_b = ClickhouseResultSink::new(
        ch.clone(),
        region_b.clone(),
        "agent-b".into(),
        OrgTtlDays::new(),
    );

    // Region A: 3 checks, 2 up. Region B: 1 check, down.
    sink_a
        .write_batch(&[
            result(target, org, CheckStatus::Up, 10),
            result(target, org, CheckStatus::Up, 20),
            result(target, org, CheckStatus::Down, 30),
        ])
        .await
        .expect("seed region A");
    sink_b
        .write_batch(&[result(target, org, CheckStatus::Down, 99)])
        .await
        .expect("seed region B");

    let stats = region_health::collect(&ch, 3600).await.expect("collect");

    let a = stats
        .iter()
        .find(|s| s.region == region_a)
        .expect("region A present");
    assert_eq!(a.total, 3, "region A total");
    assert_eq!(a.up, 2, "region A up count");
    assert!(a.p95_ms > 0.0, "region A p95 should be set");

    let b = stats
        .iter()
        .find(|s| s.region == region_b)
        .expect("region B present");
    assert_eq!(b.total, 1, "region B total");
    assert_eq!(b.up, 0, "region B up count");
}
