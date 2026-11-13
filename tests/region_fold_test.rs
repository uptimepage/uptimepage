//! ClickHouse integration test for one monitor's region fold, the status its
//! detail page, live refresh, incidents tab and share link all show.
//!
//!     CLICKHOUSE_URL=http://127.0.0.1:8123 \
//!       cargo nextest run --test it --run-ignored only -E 'test(/^region_fold_test::/)'

use crate::common;

use chrono::{Duration, Timelike, Utc};
use uptimepage::domain::{CheckResult, CheckStatus, OrgId, RegionIncidentPolicy, Target};
use uptimepage::storage::{
    ClickhouseResultSink, ClickhouseResultsStore, OrgTtlDays, ResultSink, TimeRange,
};
use uptimepage::targets::folded_status_of;
use uuid::Uuid;

fn result(target: &Target, org: Uuid, minutes_ago: i64, status: CheckStatus) -> CheckResult {
    let at = (Utc::now() - Duration::minutes(minutes_ago))
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap();
    CheckResult {
        status,
        timestamp: at,
        ..CheckResult::error(target.id, org, "")
    }
}

fn last_week() -> TimeRange {
    let to = Utc::now() + Duration::minutes(1);
    TimeRange {
        from: to - Duration::days(7),
        to,
    }
}

fn target(policy: RegionIncidentPolicy) -> Target {
    Target {
        region_policy: policy,
        ..common::http_target("127.0.0.1:9".parse().unwrap(), "/", 60_000)
    }
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn the_badge_folds_each_regions_latest_result_by_the_monitors_rule() {
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
    let majority = target(RegionIncidentPolicy::Majority);

    for region in ["r1", "r2", "r3", "r4", "r5"] {
        sink.write_batch_tagged(&[result(&majority, org, 10, CheckStatus::Up)], region, "a")
            .await
            .unwrap();
    }
    sink.write_batch_tagged(&[result(&majority, org, 1, CheckStatus::Down)], "r5", "a")
        .await
        .unwrap();
    assert_eq!(
        folded_status_of(&store, OrgId(org), last_week(), &majority).await,
        Some(CheckStatus::Degraded),
        "one region of five down is below a majority, whichever reported last"
    );

    for region in ["r3", "r4"] {
        sink.write_batch_tagged(&[result(&majority, org, 1, CheckStatus::Down)], region, "a")
            .await
            .unwrap();
    }
    assert_eq!(
        folded_status_of(&store, OrgId(org), last_week(), &majority).await,
        Some(CheckStatus::Down)
    );
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn a_region_silent_for_a_day_no_longer_votes() {
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
    let any = target(RegionIncidentPolicy::Any);

    sink.write_batch_tagged(
        &[result(&any, org, 25 * 60, CheckStatus::Down)],
        "gone",
        "a",
    )
    .await
    .unwrap();
    sink.write_batch_tagged(&[result(&any, org, 5, CheckStatus::Up)], "live", "a")
        .await
        .unwrap();
    assert_eq!(
        folded_status_of(&store, OrgId(org), last_week(), &any).await,
        Some(CheckStatus::Up)
    );
}
