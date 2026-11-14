use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uptimepage::domain::{CheckResult, CheckStatus};
use uptimepage::error::Result as SmResult;
use uptimepage::pipeline::{BatcherConfig, ResultBatcher};
use uptimepage::storage::{InMemorySink, ResultSink};
use uuid::Uuid;

fn sample_result() -> CheckResult {
    CheckResult {
        target_id: Uuid::now_v7(),
        org_id: uuid::Uuid::nil(),
        timestamp: Utc::now(),
        status: CheckStatus::Up,
        duration_ms: 10,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: Some(200),
        response_size: Some(4),
        network: None,
        diagnostic: None,
        error: None,
        details: None,
    }
}

#[tokio::test]
async fn flushes_on_size_trigger() {
    let (tx, rx) = mpsc::channel(64);
    let sink = Arc::new(InMemorySink::new());
    let cfg = BatcherConfig {
        batch_size: 3,
        batch_timeout: Duration::from_secs(60),
    };
    let batcher = ResultBatcher::new(rx, sink.clone(), cfg);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(batcher.run(shutdown.clone()));

    for _ in 0..3 {
        tx.send(sample_result()).await.unwrap();
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline && sink.len() < 3 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(sink.len(), 3);

    shutdown.cancel();
    drop(tx);
    handle.await.unwrap();
}

#[tokio::test]
async fn flushes_on_timeout_trigger() {
    let (tx, rx) = mpsc::channel(64);
    let sink = Arc::new(InMemorySink::new());
    let cfg = BatcherConfig {
        batch_size: 100,
        batch_timeout: Duration::from_millis(100),
    };
    let batcher = ResultBatcher::new(rx, sink.clone(), cfg);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(batcher.run(shutdown.clone()));

    tx.send(sample_result()).await.unwrap();
    tx.send(sample_result()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline && sink.len() < 2 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(sink.len(), 2);

    shutdown.cancel();
    drop(tx);
    handle.await.unwrap();
}

#[tokio::test]
async fn drains_on_shutdown() {
    let (tx, rx) = mpsc::channel(64);
    let sink = Arc::new(InMemorySink::new());
    let cfg = BatcherConfig {
        batch_size: 1000,
        batch_timeout: Duration::from_secs(60),
    };
    let batcher = ResultBatcher::new(rx, sink.clone(), cfg);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(batcher.run(shutdown.clone()));

    for _ in 0..5 {
        tx.send(sample_result()).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(sink.len(), 0, "no premature flush expected");

    shutdown.cancel();
    handle.await.unwrap();
    assert_eq!(sink.len(), 5, "shutdown should drain pending results");
}

/// Sink that fails the first `fail_first` flush attempts, then succeeds.
/// Records every batch seen (including failed attempts) so the test can assert
/// retry semantics.
struct FlakySink {
    fail_first: AtomicU32,
    seen: Mutex<Vec<Vec<CheckResult>>>,
}

impl FlakySink {
    fn new(fail_first: u32) -> Self {
        Self {
            fail_first: AtomicU32::new(fail_first),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn writes_succeeded(&self) -> Vec<Vec<CheckResult>> {
        self.seen
            .lock()
            .iter()
            .filter(|b| !b.is_empty())
            .cloned()
            .collect()
    }
}

#[async_trait]
impl ResultSink for FlakySink {
    async fn write_batch(&self, results: &[CheckResult]) -> SmResult<()> {
        self.seen.lock().push(results.to_vec());
        if self.fail_first.load(Ordering::SeqCst) > 0 {
            self.fail_first.fetch_sub(1, Ordering::SeqCst);
            return Err(anyhow::anyhow!("synthetic flush failure").into());
        }
        Ok(())
    }
}

#[tokio::test]
async fn flush_failure_retains_buffer_for_next_attempt() {
    // First attempt fails, second succeeds. The same rows must reach the sink
    // on the second attempt — no `buffer.clear()` after the failed Err arm.
    let (tx, rx) = mpsc::channel(64);
    let sink = Arc::new(FlakySink::new(1));
    let cfg = BatcherConfig {
        batch_size: 2,
        batch_timeout: Duration::from_millis(80),
    };
    let batcher = ResultBatcher::new(rx, sink.clone(), cfg);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(batcher.run(shutdown.clone()));

    let a = sample_result();
    let b = sample_result();
    tx.send(a.clone()).await.unwrap();
    tx.send(b.clone()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut succeeded = Vec::new();
    while tokio::time::Instant::now() < deadline {
        succeeded = sink.writes_succeeded();
        if succeeded.iter().any(|batch| batch.len() == 2) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    shutdown.cancel();
    drop(tx);
    handle.await.unwrap();

    let retained = succeeded
        .iter()
        .find(|batch| batch.len() == 2)
        .expect("a 2-row batch must have eventually been written");
    assert_eq!(retained[0].target_id, a.target_id);
    assert_eq!(retained[1].target_id, b.target_id);
}

#[tokio::test]
async fn a_failed_batch_is_resent_unchanged_while_new_results_wait() {
    let (tx, rx) = mpsc::channel(64);
    let sink = Arc::new(FlakySink::new(1));
    let cfg = BatcherConfig {
        batch_size: 2,
        batch_timeout: Duration::from_millis(80),
    };
    let results: Vec<_> = (0..5).map(|_| sample_result()).collect();
    for r in &results {
        tx.send(r.clone()).await.unwrap();
    }
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(ResultBatcher::new(rx, sink.clone(), cfg).run(shutdown.clone()));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while sink.seen.lock().len() < 4 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "writes so far: {}",
            sink.seen.lock().len()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    shutdown.cancel();
    handle.await.unwrap();

    let ids = |batch: &[CheckResult]| batch.iter().map(|r| r.target_id).collect::<Vec<_>>();
    let seen: Vec<_> = sink.seen.lock().iter().map(|batch| ids(batch)).collect();
    let (ab, cd, e) = (ids(&results[..2]), ids(&results[2..4]), ids(&results[4..]));
    assert_eq!(seen, [ab.clone(), ab, cd, e]);
}

#[tokio::test]
async fn flushes_when_sender_dropped() {
    let (tx, rx) = mpsc::channel(64);
    let sink = Arc::new(InMemorySink::new());
    let cfg = BatcherConfig {
        batch_size: 1000,
        batch_timeout: Duration::from_secs(60),
    };
    let batcher = ResultBatcher::new(rx, sink.clone(), cfg);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(batcher.run(shutdown.clone()));

    tx.send(sample_result()).await.unwrap();
    tx.send(sample_result()).await.unwrap();
    drop(tx);

    handle.await.unwrap();
    assert_eq!(sink.len(), 2);
}
