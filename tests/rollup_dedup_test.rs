use std::time::{Duration as StdDuration, Instant};

use chrono::{Duration, Utc};
use tokio_util::sync::CancellationToken;
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

fn sink(client: clickhouse::Client) -> ClickhouseResultSink {
    ClickhouseResultSink::new(
        client,
        "default".into(),
        "default".into(),
        OrgTtlDays::new(),
    )
}

fn async_inserts(ch: &clickhouse::Client) -> clickhouse::Client {
    ch.clone()
        .with_setting("async_insert", "1")
        .with_setting("wait_for_async_insert", "1")
        .with_setting("async_insert_deduplicate", "1")
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
    let sinks = [
        sink(ch.clone()),
        sink(async_inserts(&ch)),
        sink(async_inserts(&ch))
            .with_async_coalescing(StdDuration::from_millis(300), CancellationToken::new()),
    ];

    for (variant, sink) in sinks.into_iter().enumerate() {
        let target_id = Uuid::now_v7();
        let rows = [result(target_id, 30), result(target_id, 60)];

        sink.write_batch(&rows).await.expect("first write");
        sink.write_batch(&rows).await.expect("repeated write");

        for table in ["check_results_1m", "check_results_1h"] {
            assert_eq!(
                rollup_total(&ch, table, target_id).await,
                2,
                "{table} variant {variant}"
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires CLICKHOUSE_URL"]
async fn only_a_small_batch_before_shutdown_waits_out_the_window() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };
    let window = StdDuration::from_secs(4);
    let shutdown = CancellationToken::new();
    let sink = sink(async_inserts(&ch)).with_async_coalescing(window, shutdown.clone());
    let target_id = Uuid::now_v7();
    let ack_time = async |rows: &[CheckResult]| {
        let started = Instant::now();
        sink.write_batch(rows).await.expect("write");
        started.elapsed()
    };

    let small = ack_time(&[result(target_id, 30)]).await;
    let large: Vec<_> = (0..500).map(|i| result(target_id, 60 + i)).collect();
    let large = ack_time(&large).await;
    shutdown.cancel();
    let last = ack_time(&[result(target_id, 600)]).await;

    assert!(
        small >= window - StdDuration::from_millis(500),
        "small batch acked after {small:?}"
    );
    assert!(large < window / 2, "large batch acked after {large:?}");
    assert!(
        last < window / 2,
        "batch after shutdown acked after {last:?}"
    );
}

#[tokio::test]
#[ignore = "requires CLICKHOUSE_URL"]
async fn a_resend_merged_with_other_posts_still_counts_once_in_the_rollups() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };
    let sink = sink(async_inserts(&ch))
        .with_async_coalescing(StdDuration::from_millis(500), CancellationToken::new());
    let ids: Vec<[Uuid; 3]> = (0..5)
        .map(|_| std::array::from_fn(|_| Uuid::now_v7()))
        .collect();
    let batches: Vec<_> = ids
        .iter()
        .map(|ids| ids.map(|t| [result(t, 30), result(t, 60)]))
        .collect();
    let resend_beside = async |neighbour: usize| {
        let writes = batches
            .iter()
            .flat_map(|b| [sink.write_batch(&b[0]), sink.write_batch(&b[neighbour])]);
        futures::future::try_join_all(writes).await
    };

    resend_beside(1).await.expect("first flush");
    let started = Instant::now();
    resend_beside(2).await.expect("second flush");
    let took = started.elapsed();
    assert!(
        took < StdDuration::from_secs(5),
        "second flush took {took:?}"
    );

    for table in ["check_results_1m", "check_results_1h"] {
        for target in ids.iter().flatten() {
            assert_eq!(
                rollup_total(&ch, table, *target).await,
                2,
                "{table} {target}"
            );
        }
    }
}
