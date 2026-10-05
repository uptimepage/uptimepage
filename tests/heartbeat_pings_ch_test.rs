//! Storage contract for the heartbeat ping log.

use crate::common;

use clickhouse::Row;
use serde::Deserialize;
use uuid::Uuid;

use uptimepage::domain::{HeartbeatPingRecord, PingSignal};
use uptimepage::storage::ClickhouseHeartbeatPingSink;
use uptimepage::storage::traits::{HeartbeatPingSink, ResultsStore};

#[derive(Debug, Row, Deserialize)]
struct StoredPing {
    signal: i8,
    exit_code: Option<u8>,
    duration_ms: Option<u32>,
    body: String,
}

async fn fetch(client: &clickhouse::Client, org_id: Uuid) -> Vec<StoredPing> {
    client
        .query(
            "SELECT signal, exit_code, duration_ms, body FROM heartbeat_pings \
             WHERE org_id = ? ORDER BY received_at, signal",
        )
        .bind(org_id)
        .fetch_all::<StoredPing>()
        .await
        .expect("read heartbeat_pings")
}

fn ping(org_id: Uuid, target_id: Uuid, signal: PingSignal) -> HeartbeatPingRecord {
    HeartbeatPingRecord {
        org_id,
        target_id,
        received_at: chrono::Utc::now(),
        signal,
        exit_code: None,
        duration_ms: None,
        body: String::new(),
    }
}

#[tokio::test]
#[ignore]
async fn a_run_stores_its_start_its_outcome_and_its_output() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let org_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let sink =
        ClickhouseHeartbeatPingSink::new(client.clone(), uptimepage::storage::OrgTtlDays::new());

    sink.write_ping(&ping(org_id, target_id, PingSignal::Start))
        .await;
    sink.write_ping(&HeartbeatPingRecord {
        exit_code: Some(137),
        duration_ms: Some(94_000),
        body: "rsync: connection unexpectedly closed".into(),
        ..ping(org_id, target_id, PingSignal::Fail)
    })
    .await;

    let rows = fetch(&client, org_id).await;
    assert_eq!(rows.len(), 2, "both signals kept, not just the verdict");

    let start = &rows[0];
    assert_eq!(start.signal, PingSignal::Start.as_enum8());
    assert_eq!(start.duration_ms, None);
    assert_eq!(start.exit_code, None);

    let fail = &rows[1];
    assert_eq!(fail.signal, PingSignal::Fail.as_enum8());
    assert_eq!(fail.exit_code, Some(137));
    assert_eq!(fail.duration_ms, Some(94_000));
    assert!(fail.body.contains("rsync"));
}

/// Absent, not zero — 0 is a real exit status.
#[tokio::test]
#[ignore]
async fn a_bare_success_stores_no_exit_code() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let org_id = Uuid::new_v4();
    let sink =
        ClickhouseHeartbeatPingSink::new(client.clone(), uptimepage::storage::OrgTtlDays::new());
    sink.write_ping(&ping(org_id, Uuid::new_v4(), PingSignal::Success))
        .await;

    let rows = fetch(&client, org_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].signal, PingSignal::Success.as_enum8());
    assert_eq!(rows[0].exit_code, None);
    assert_eq!(rows[0].duration_ms, None);
}

/// Matched to the exact instant Postgres holds, not "the newest failure" — a
/// lost log write must read as absent, never as the previous run's output.
#[tokio::test]
#[ignore]
async fn failure_output_is_matched_to_its_own_failure() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let org = uptimepage::domain::OrgId(Uuid::new_v4());
    let target = Uuid::new_v4();
    let sink =
        ClickhouseHeartbeatPingSink::new(client.clone(), uptimepage::storage::OrgTtlDays::new());
    let store = uptimepage::storage::ClickhouseResultsStore::from_client(client);

    let older = chrono::Utc::now() - chrono::Duration::hours(2);
    sink.write_ping(&HeartbeatPingRecord {
        received_at: older,
        exit_code: Some(1),
        body: "disk full".into(),
        ..ping(org.0, target, PingSignal::Fail)
    })
    .await;

    assert_eq!(
        store
            .heartbeat_failure_output(org, target, older)
            .await
            .unwrap()
            .as_deref(),
        Some("disk full")
    );

    let newer = chrono::Utc::now();
    assert_eq!(
        store
            .heartbeat_failure_output(org, target, newer)
            .await
            .unwrap(),
        None
    );

    let stranger = uptimepage::domain::OrgId(Uuid::new_v4());
    assert_eq!(
        store
            .heartbeat_failure_output(stranger, target, older)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
#[ignore]
async fn observed_cadence_contradicts_a_wrong_declaration() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let org = uptimepage::domain::OrgId(Uuid::new_v4());
    let target = Uuid::new_v4();
    let sink =
        ClickhouseHeartbeatPingSink::new(client.clone(), uptimepage::storage::OrgTtlDays::new());
    let store = uptimepage::storage::ClickhouseResultsStore::from_client(client);

    // Runs every ~80 minutes, declared as 10.
    let base = chrono::Utc::now() - chrono::Duration::days(2);
    for (i, offset_min) in [0i64, 83, 163, 245, 325, 408, 488].iter().enumerate() {
        sink.write_ping(&HeartbeatPingRecord {
            received_at: base + chrono::Duration::minutes(*offset_min),
            ..ping(org.0, target, PingSignal::Success)
        })
        .await;
        // Starts must not count as schedule ticks.
        if i.is_multiple_of(2) {
            sink.write_ping(&HeartbeatPingRecord {
                received_at: base + chrono::Duration::minutes(offset_min - 1),
                ..ping(org.0, target, PingSignal::Start)
            })
            .await;
        }
    }

    let seen = store
        .heartbeat_cadence(org, target, 14)
        .await
        .unwrap()
        .expect("seven successes give six gaps");
    assert_eq!(seen.samples, 6, "gaps, with no start among them");
    // Gaps 83/80/82/80/83/80; quantileExact takes the upper of the middle pair.
    assert_eq!(seen.median_gap.as_secs(), 82 * 60);

    // Declared 10m with 5m grace: still nowhere near the real 82m cadence.
    match seen.advice(std::time::Duration::from_secs(900)) {
        Some(uptimepage::domain::CadenceAdvice::TooTight { suggested_period }) => {
            assert_eq!(suggested_period.as_secs(), 90 * 60)
        }
        other => panic!("expected a too-tight verdict, got {other:?}"),
    }

    assert_eq!(seen.advice(std::time::Duration::from_secs(90 * 60)), None);

    let stranger = uptimepage::domain::OrgId(Uuid::new_v4());
    assert_eq!(
        store.heartbeat_cadence(stranger, target, 14).await.unwrap(),
        None
    );
}

/// The count behind "N pings received": its window, its tenant, and the rule
/// that a start is a run announcing itself rather than another report.
#[tokio::test]
#[ignore]
async fn the_ping_count_holds_its_window_its_tenant_and_skips_starts() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let org = uptimepage::domain::OrgId(Uuid::new_v4());
    let neighbour = uptimepage::domain::OrgId(Uuid::new_v4());
    let target = Uuid::new_v4();
    let sink =
        ClickhouseHeartbeatPingSink::new(client.clone(), uptimepage::storage::OrgTtlDays::new());
    let store = uptimepage::storage::ClickhouseResultsStore::from_client(client);

    let from = chrono::Utc::now() - chrono::Duration::hours(2);
    let to = chrono::Utc::now() - chrono::Duration::hours(1);
    let at = |minutes: i64| from + chrono::Duration::minutes(minutes);
    for (received_at, signal) in [
        (at(-1), PingSignal::Success), // before the window
        (from, PingSignal::Success),   // the window includes its start
        (at(20), PingSignal::Start),   // the run, announcing itself
        (at(21), PingSignal::Fail),    // the same run, reporting
        (to, PingSignal::Success),     // the window excludes its end
        (at(70), PingSignal::Success), // after the window
    ] {
        sink.write_ping(&HeartbeatPingRecord {
            received_at,
            ..ping(org.0, target, signal)
        })
        .await;
    }
    // Another tenant, same target id: the count must not see it.
    sink.write_ping(&HeartbeatPingRecord {
        received_at: at(30),
        ..ping(neighbour.0, target, PingSignal::Success)
    })
    .await;

    let range =
        uptimepage::storage::ClampedRange::unclamped(uptimepage::storage::TimeRange { from, to });
    let counted = store
        .heartbeat_ping_count(org, target, range)
        .await
        .unwrap()
        .expect("clickhouse answers with a count");
    assert_eq!(
        counted, 2,
        "one success and one fail, no start, no neighbour"
    );
}
