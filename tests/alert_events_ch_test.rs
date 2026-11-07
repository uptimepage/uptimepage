//! Storage contract for an alert source's delivery log.

use crate::common;

use clickhouse::Row;
use serde::Deserialize;
use uuid::Uuid;

use uptimepage::storage::ClickhouseAlertEventSink;
use uptimepage::storage::alert_ingest::{Delivery, Status};
use uptimepage::storage::traits::AlertEventSink;

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
