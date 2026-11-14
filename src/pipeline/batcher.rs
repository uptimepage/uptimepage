use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use metrics::{counter, histogram};
use tokio::sync::mpsc;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

use crate::domain::CheckResult;
use crate::metric_names;
use crate::storage::ResultSink;

const MAX_FLUSH_RETRIES: u32 = 3;
const MAX_BUFFER_MULT: usize = 8;

#[derive(Debug, Clone, Copy)]
pub struct BatcherConfig {
    pub batch_size: usize,
    pub batch_timeout: Duration,
}

/// The batch being written. A failed one is resent unchanged until it lands or
/// is dropped: grown by the rows queued behind it, it would be another block,
/// which insert deduplication cannot match to a copy that did commit.
#[derive(Default)]
struct Batch {
    rows: Vec<CheckResult>,
    attempts: u32,
}

pub struct ResultBatcher {
    rx: mpsc::Receiver<CheckResult>,
    sink: Arc<dyn ResultSink>,
    cfg: BatcherConfig,
}

impl ResultBatcher {
    pub fn new(
        rx: mpsc::Receiver<CheckResult>,
        sink: Arc<dyn ResultSink>,
        cfg: BatcherConfig,
    ) -> Self {
        Self { rx, sink, cfg }
    }

    pub async fn run(mut self, shutdown: CancellationToken) {
        let max_buffer = self.cfg.batch_size.saturating_mul(MAX_BUFFER_MULT);
        let mut buffer: VecDeque<CheckResult> = VecDeque::with_capacity(self.cfg.batch_size);
        let mut batch = Batch::default();
        let mut ticker = interval(self.cfg.batch_timeout);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    drain(&mut self.rx, &mut buffer, max_buffer);
                    finish(&self.sink, &mut buffer, &mut batch, self.cfg.batch_size).await;
                    return;
                }
                maybe_result = self.rx.recv() => {
                    let Some(result) = maybe_result else {
                        finish(&self.sink, &mut buffer, &mut batch, self.cfg.batch_size).await;
                        return;
                    };
                    push_capped(&mut buffer, result, max_buffer);
                    if batch.rows.is_empty() && buffer.len() >= self.cfg.batch_size {
                        flush(&self.sink, &mut buffer, &mut batch, self.cfg.batch_size).await;
                    }
                }
                _ = ticker.tick() => {
                    flush_all(&self.sink, &mut buffer, &mut batch, self.cfg.batch_size).await;
                }
            }
        }
    }
}

fn drain(
    rx: &mut mpsc::Receiver<CheckResult>,
    buffer: &mut VecDeque<CheckResult>,
    max_buffer: usize,
) {
    while let Ok(r) = rx.try_recv() {
        push_capped(buffer, r, max_buffer);
    }
}

fn push_capped(buffer: &mut VecDeque<CheckResult>, result: CheckResult, max_buffer: usize) {
    if buffer.len() >= max_buffer {
        buffer.pop_front();
        counter!(metric_names::STORAGE_DROPPED, "reason" => "buffer_overflow").increment(1);
    }
    buffer.push_back(result);
}

/// The failed batch first, then what queued behind it, until a write fails.
async fn flush_all(
    sink: &Arc<dyn ResultSink>,
    buffer: &mut VecDeque<CheckResult>,
    batch: &mut Batch,
    batch_size: usize,
) {
    while !(batch.rows.is_empty() && buffer.is_empty()) {
        flush(sink, buffer, batch, batch_size).await;
        if !batch.rows.is_empty() {
            return;
        }
    }
}

async fn finish(
    sink: &Arc<dyn ResultSink>,
    buffer: &mut VecDeque<CheckResult>,
    batch: &mut Batch,
    batch_size: usize,
) {
    flush_all(sink, buffer, batch, batch_size).await;
    let lost = batch.rows.len() + buffer.len();
    if lost > 0 {
        tracing::error!(lost, "batcher stopped with results it could not write");
        counter!(metric_names::STORAGE_DROPPED, "reason" => "shutdown_unwritten")
            .increment(lost as u64);
    }
}

async fn flush(
    sink: &Arc<dyn ResultSink>,
    buffer: &mut VecDeque<CheckResult>,
    batch: &mut Batch,
    batch_size: usize,
) {
    if batch.rows.is_empty() {
        let take = buffer.len().min(batch_size);
        batch.rows.extend(buffer.drain(..take));
    }
    if batch.rows.is_empty() {
        return;
    }
    let count = batch.rows.len();
    histogram!(metric_names::STORAGE_BATCH_SIZE).record(count as f64);
    let start = Instant::now();
    match sink.write_batch(&batch.rows).await {
        Ok(()) => {
            counter!(metric_names::STORAGE_WRITES, "store" => "sink", "result" => "success")
                .increment(1);
            batch.rows.clear();
            batch.attempts = 0;
        }
        Err(err) => {
            counter!(metric_names::STORAGE_WRITES, "store" => "sink", "result" => "failure")
                .increment(1);
            batch.attempts = batch.attempts.saturating_add(1);
            if batch.attempts >= MAX_FLUSH_RETRIES {
                tracing::error!(
                    ?err,
                    count,
                    retries = batch.attempts,
                    "batcher flush failed; dropping batch after exhausted retries"
                );
                counter!(metric_names::STORAGE_DROPPED, "reason" => "retries_exhausted")
                    .increment(count as u64);
                batch.rows.clear();
                batch.attempts = 0;
            } else {
                tracing::warn!(
                    ?err,
                    count,
                    attempt = batch.attempts,
                    "batcher flush failed; resending the same batch on next tick"
                );
            }
        }
    }
    histogram!(metric_names::STORAGE_WRITE_DURATION_MS).record(start.elapsed().as_millis() as f64);
}
