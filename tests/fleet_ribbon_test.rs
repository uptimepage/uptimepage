//! ClickHouse integration test for the dashboard fleet ribbon's per-cell
//! down-monitor set. Validates what in-memory unit tests can't: the
//! `groupUniqArrayIf(target_id, …)` aggregate and the `Array(UUID)` →
//! `Vec<Uuid>` decode round-trip (the crate's UUID serde adapter is
//! single-value only, so the array is read in its raw `(hi, lo)` form).
//!
//! Skipped by default — requires a ClickHouse the migrations have run against:
//!
//!     CLICKHOUSE_URL=http://127.0.0.1:8123 \
//!       cargo nextest run --test it --run-ignored only --no-capture -E 'test(/^fleet_ribbon_test::/)'

use crate::common;

use chrono::{Duration, Timelike, Utc};
use uptimepage::domain::{CheckResult, CheckStatus, OrgId};
use uptimepage::storage::{
    ClickhouseResultSink, ClickhouseResultsStore, OrgTtlDays, ResultSink, ResultsStore,
};
use uuid::Uuid;

fn result(target: Uuid, org: Uuid, ts: chrono::DateTime<Utc>, status: CheckStatus) -> CheckResult {
    CheckResult {
        target_id: target,
        org_id: org,
        timestamp: ts,
        status,
        duration_ms: 50,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: None,
        response_size: None,
        diagnostic: None,
        error: (status != CheckStatus::Up).then(|| "no response".to_string()),
    }
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn fleet_ribbon_collects_and_decodes_down_targets() {
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
    let healthy = Uuid::now_v7();
    let dipped = Uuid::now_v7();
    let base = (Utc::now() - Duration::hours(2))
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();

    let mut rows: Vec<CheckResult> = (0..3)
        .map(|i| {
            result(
                healthy,
                org,
                base + Duration::seconds(i * 5),
                CheckStatus::Up,
            )
        })
        .collect();
    // The dipped monitor has one up and one down sample in the same minute,
    // so it qualifies on the `samples > up` (any non-up) rule, not all-down.
    rows.push(result(dipped, org, base, CheckStatus::Up));
    rows.push(result(
        dipped,
        org,
        base + Duration::seconds(5),
        CheckStatus::Down,
    ));
    sink.write_batch(&rows).await.expect("insert samples");

    let buckets = store
        .fleet_ribbon(
            OrgId(org),
            base - Duration::minutes(1),
            base + Duration::minutes(5),
            1800,
            None,
        )
        .await
        .expect("fleet_ribbon query");

    let down: Vec<Uuid> = buckets
        .iter()
        .flat_map(|b| b.down_targets.clone())
        .collect();
    assert!(
        down.contains(&dipped),
        "the dipped monitor's UUID decodes and is collected: {down:?}"
    );
    assert!(
        !down.contains(&healthy),
        "a fully-up monitor never appears in down_targets: {down:?}"
    );

    let sampled = store
        .sampled_targets(
            OrgId(org),
            base - Duration::minutes(1),
            base + Duration::minutes(5),
        )
        .await
        .expect("sampled_targets query");
    assert_eq!(
        sampled,
        [healthy, dipped].into_iter().collect(),
        "every checked monitor, healthy or not"
    );
}
