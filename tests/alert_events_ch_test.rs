//! Storage contract for an alert source's delivery log.

use crate::common;

use clickhouse::Row;
use serde::Deserialize;
use uuid::Uuid;

use uptimepage::domain::OrgId;
use uptimepage::storage::ClickhouseAlertEventSink;
use uptimepage::storage::alert_delivery::{Delivery, Status};
use uptimepage::storage::traits::{AlertEventCounts, AlertEventLog, AlertEventSink};

#[derive(Debug, Row, Deserialize)]
struct Stored {
    outcome: i8,
    dedup_key: String,
    #[serde(with = "clickhouse::serde::uuid::option")]
    incident_id: Option<Uuid>,
    note: String,
    body: String,
    received_ms: i64,
}

fn delivery(org_id: Uuid, source_id: Uuid, status: Status) -> Delivery {
    Delivery {
        org_id,
        source_id,
        received_at: chrono::Utc::now(),
        status,
        dedup_key: "checkout-5xx".into(),
        incident_id: None,
        note: String::new(),
        body: String::new(),
    }
}

#[tokio::test]
#[ignore]
async fn a_request_stores_one_row_per_event_and_skips_a_deleted_source() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let (org_id, source_id, incident) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let sink =
        ClickhouseAlertEventSink::new(client.clone(), uptimepage::storage::OrgTtlDays::new());
    let first = Delivery {
        incident_id: Some(incident),
        body: "{\"event_action\":\"trigger\"}".into(),
        ..delivery(org_id, source_id, Status::Opened)
    };
    sink.write_deliveries(&[
        first.clone(),
        Delivery {
            received_at: first.received_at,
            dedup_key: "disk-full".into(),
            note: "nothing open for this key".into(),
            ..delivery(org_id, source_id, Status::Ignored)
        },
        Delivery {
            received_at: first.received_at,
            ..delivery(org_id, source_id, Status::Gone)
        },
    ])
    .await;

    let rows = client
        .query(
            "SELECT toInt8(outcome) AS outcome, dedup_key, incident_id, note, body, \
             toUnixTimestamp64Milli(received_at) AS received_ms FROM alert_events \
             WHERE org_id = ? AND source_id = ? ORDER BY received_at, outcome",
        )
        .bind(org_id)
        .bind(source_id)
        .fetch_all::<Stored>()
        .await
        .expect("read alert_events");
    assert_eq!(rows.len(), 2, "a deleted source's event is not logged");
    assert_eq!(rows[0].outcome, 1);
    assert_eq!(rows[0].incident_id, Some(incident));
    assert!(rows[0].body.contains("trigger"));
    assert_eq!(rows[0].received_ms, first.received_at.timestamp_millis());
    assert_eq!(rows[1].outcome, 4);
    assert_eq!(rows[1].dedup_key, "disk-full");
    assert_eq!(rows[1].note, "nothing open for this key");
    assert_eq!(rows[1].incident_id, None);
}

#[tokio::test]
#[ignore]
async fn the_log_reads_back_newest_first_a_page_at_a_time_and_only_its_own() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let (org_id, source_id) = (Uuid::new_v4(), Uuid::new_v4());
    let log = ClickhouseAlertEventSink::new(client, uptimepage::storage::OrgTtlDays::new());
    let start = chrono::Utc::now() - chrono::Duration::minutes(1);
    for (secs, status) in [
        (0, Status::Opened),
        (1, Status::Repeat),
        (2, Status::Resolved),
    ] {
        log.write_deliveries(&[Delivery {
            received_at: start + chrono::Duration::seconds(secs),
            body: if secs == 0 {
                "{\"event_action\":\"trigger\"}".into()
            } else {
                String::new()
            },
            ..delivery(org_id, source_id, status)
        }])
        .await;
    }
    log.write_deliveries(&[delivery(org_id, Uuid::new_v4(), Status::Opened)])
        .await;

    let page = log
        .recent(OrgId(org_id), source_id, &[], None, 2)
        .await
        .unwrap();
    let outcomes: Vec<Status> = page.iter().map(|e| e.outcome).collect();
    assert_eq!(outcomes, [Status::Resolved, Status::Repeat]);
    let next = log
        .recent(
            OrgId(org_id),
            source_id,
            &[],
            Some((page[1].received_at, &page[1].dedup_key)),
            2,
        )
        .await
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].outcome, Status::Opened);
    assert_eq!(next[0].body, "{\"event_action\":\"trigger\"}");
    assert!(
        log.recent(OrgId(Uuid::new_v4()), source_id, &[], None, 10)
            .await
            .unwrap()
            .is_empty(),
        "another org reads nothing of it"
    );
}

#[tokio::test]
#[ignore]
async fn a_page_can_end_inside_a_request_without_losing_its_other_events() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let (org_id, source_id) = (Uuid::new_v4(), Uuid::new_v4());
    let log = ClickhouseAlertEventSink::new(client, uptimepage::storage::OrgTtlDays::new());
    let at = chrono::Utc::now() - chrono::Duration::minutes(1);
    let rows: Vec<Delivery> = ["c", "a", "b"]
        .into_iter()
        .map(|key| Delivery {
            received_at: at,
            dedup_key: key.into(),
            ..delivery(org_id, source_id, Status::Opened)
        })
        .collect();
    log.write_deliveries(&rows).await;

    let first = log
        .recent(OrgId(org_id), source_id, &[], None, 2)
        .await
        .unwrap();
    let keys: Vec<&str> = first.iter().map(|e| e.dedup_key.as_str()).collect();
    assert_eq!(keys, ["a", "b"]);
    let rest = log
        .recent(
            OrgId(org_id),
            source_id,
            &[],
            Some((first[1].received_at, "b")),
            2,
        )
        .await
        .unwrap();
    let keys: Vec<&str> = rest.iter().map(|e| e.dedup_key.as_str()).collect();
    assert_eq!(keys, ["c"], "the request's last event is on the next page");
}

#[tokio::test]
#[ignore]
async fn a_row_past_its_retention_reads_as_gone_before_any_merge() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let (org_id, source_id) = (Uuid::new_v4(), Uuid::new_v4());
    // With TTL merges stopped the row is still on disk, so only the read's
    // own retention filter can hide it.
    let system = |verb: &'static str| {
        let client = client.clone();
        async move {
            client
                .query(&format!("SYSTEM {verb} TTL MERGES alert_events"))
                .execute()
                .await
                .expect("ttl merges switch")
        }
    };
    // Started first, so a run killed while they were stopped heals here.
    system("START").await;
    system("STOP").await;
    let log = ClickhouseAlertEventSink::new(client.clone(), uptimepage::storage::OrgTtlDays::new());
    log.write_deliveries(&[Delivery {
        received_at: chrono::Utc::now() - chrono::Duration::days(400),
        ..delivery(org_id, source_id, Status::Opened)
    }])
    .await;
    // Restarted before anything can fail.
    let read = log.recent(OrgId(org_id), source_id, &[], None, 10).await;
    let stored = client
        .query("SELECT count() FROM alert_events WHERE org_id = ? AND source_id = ?")
        .bind(org_id)
        .bind(source_id)
        .fetch_one::<u64>()
        .await;
    system("START").await;
    assert_eq!(stored.unwrap(), 1, "the expired row is still stored");
    assert!(read.unwrap().is_empty(), "and reads as gone");
}

/// Every outcome survives the round trip, so the Enum8 in the table and the
/// codes the code writes cannot drift apart unnoticed.
#[tokio::test]
#[ignore]
async fn every_outcome_reads_back_as_written() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let (org_id, source_id) = (Uuid::new_v4(), Uuid::new_v4());
    let log = ClickhouseAlertEventSink::new(client, uptimepage::storage::OrgTtlDays::new());
    let all = [
        Status::Opened,
        Status::Repeat,
        Status::Resolved,
        Status::Ignored,
        Status::Paused,
        Status::Held,
        Status::Capped,
        Status::Unparsed,
        Status::Unavailable,
    ];
    let start = chrono::Utc::now() - chrono::Duration::minutes(1);
    for (i, status) in all.iter().enumerate() {
        log.write_deliveries(&[Delivery {
            received_at: start + chrono::Duration::seconds(i as i64),
            ..delivery(org_id, source_id, *status)
        }])
        .await;
    }
    let mut read: Vec<Status> = log
        .recent(OrgId(org_id), source_id, &[], None, 20)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.outcome)
        .collect();
    read.reverse();
    assert_eq!(read, all);
}

#[tokio::test]
#[ignore]
async fn the_log_filters_by_outcome_and_counts_what_did_not_apply() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let (org_id, source_id) = (Uuid::new_v4(), Uuid::new_v4());
    let log = ClickhouseAlertEventSink::new(client, uptimepage::storage::OrgTtlDays::new());
    let now = chrono::Utc::now();
    let written = [
        (Status::Opened, 50),
        (Status::Repeat, 40),
        (Status::Held, 30),
        (Status::Unparsed, 20),
        (Status::Resolved, 10),
    ];
    for (status, mins_ago) in written {
        log.write_deliveries(&[Delivery {
            received_at: now - chrono::Duration::minutes(mins_ago),
            ..delivery(org_id, source_id, status)
        }])
        .await;
    }
    log.write_deliveries(&[Delivery {
        received_at: now - chrono::Duration::hours(30),
        ..delivery(org_id, source_id, Status::Capped)
    }])
    .await;

    let outcomes = |page: Vec<uptimepage::storage::traits::AlertEventView>| {
        page.into_iter().map(|e| e.outcome).collect::<Vec<_>>()
    };
    let opened = log
        .recent(OrgId(org_id), source_id, &[Status::Opened], None, 10)
        .await
        .unwrap();
    assert_eq!(outcomes(opened), [Status::Opened]);
    let not_applied = log
        .recent(OrgId(org_id), source_id, &Status::NOT_APPLIED, None, 10)
        .await
        .unwrap();
    assert_eq!(
        outcomes(not_applied),
        [Status::Unparsed, Status::Held, Status::Capped]
    );

    let neighbour = Uuid::new_v4();
    log.write_deliveries(&[delivery(org_id, neighbour, Status::Opened)])
        .await;

    let since = now - chrono::Duration::hours(24);
    let hours = log.hourly(OrgId(org_id), None, since).await.unwrap();
    let of = |source: Uuid| -> AlertEventCounts {
        hours
            .iter()
            .filter(|h| h.source == source)
            .map(|h| h.counts)
            .sum()
    };
    let mine = of(source_id);
    assert_eq!(
        (mine.events, mine.not_applied),
        (5, 2),
        "the capped row is older than the window"
    );
    assert_eq!(of(neighbour).events, 1, "each source counts its own");
    assert!(
        hours
            .iter()
            .all(|h| h.hour.timestamp() % 3600 == 0 && h.hour <= now),
        "each count is keyed by its clock hour"
    );
    let only = log
        .hourly(OrgId(org_id), Some(neighbour), since)
        .await
        .unwrap();
    assert!(
        only.iter().all(|h| h.source == neighbour) && !only.is_empty(),
        "one source reads only its own hours"
    );
    let other = log
        .hourly(OrgId(Uuid::new_v4()), None, since)
        .await
        .unwrap();
    assert!(other.is_empty(), "another org counts nothing of it");
}
