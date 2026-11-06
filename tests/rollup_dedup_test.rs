use chrono::{Duration, Utc};
use uptimepage::domain::{CheckResult, CheckStatus};
use uptimepage::storage::{ClickhouseResultSink, OrgTtlDays, ResultSink};
use uuid::Uuid;

use crate::common;

fn result(target_id: Uuid, seconds_ago: i64) -> CheckResult {
    CheckResult {
        target_id,
        org_id: Uuid::nil(),
        timestamp: Utc::now() - Duration::seconds(seconds_ago),
        status: CheckStatus::Up,
        duration_ms: 10,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: Some(200),
        response_size: None,
        network: None,
        diagnostic: None,
        error: None,
        details: None,
    }
}

async fn rollup_total(ch: &clickhouse::Client, table: &str, target_id: Uuid) -> u64 {
    ch.query(&format!(
        "SELECT countMerge(total_checks) FROM {table} WHERE target_id = toUUID(?)"
    ))
    .bind(target_id.to_string())
    .fetch_one::<u64>()
    .await
    .expect("rollup total")
}

#[tokio::test]
#[ignore = "requires CLICKHOUSE_URL"]
async fn a_batch_sent_again_after_a_lost_ack_counts_once_in_the_rollups() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };
    let async_inserts = ch
        .clone()
        .with_setting("async_insert", "1")
        .with_setting("wait_for_async_insert", "1")
        .with_setting("async_insert_deduplicate", "1");

    for client in [ch.clone(), async_inserts] {
        let target_id = Uuid::now_v7();
        let rows = [result(target_id, 30), result(target_id, 60)];
        let sink = ClickhouseResultSink::new(
            client,
            "default".into(),
            "default".into(),
            OrgTtlDays::new(),
        );

        sink.write_batch(&rows).await.expect("first write");
        sink.write_batch(&rows).await.expect("repeated write");

        for table in ["check_results_1m", "check_results_1h"] {
            assert_eq!(rollup_total(&ch, table, target_id).await, 2, "{table}");
        }
    }
}
