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
        let mut retry_attempts: u32 = 0;
        let mut ticker = interval(self.cfg.batch_timeout);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    drain(&mut self.rx, &mut buffer, max_buffer);
                    if !buffer.is_empty() {
                        flush(&self.sink, &mut buffer, &mut retry_attempts).await;
                    }
                    return;
                }
                maybe_result = self.rx.recv() => {
                    let Some(result) = maybe_result else {
                        if !buffer.is_empty() {
                            flush(&self.sink, &mut buffer, &mut retry_attempts).await;
                        }
                        return;
                    };
                    push_capped(&mut buffer, result, max_buffer);
                    if buffer.len() >= self.cfg.batch_size {
                        flush(&self.sink, &mut buffer, &mut retry_attempts).await;
                    }
                }
                _ = ticker.tick() => {
                    if !buffer.is_empty() {
                        flush(&self.sink, &mut buffer, &mut retry_attempts).await;
                    }
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

async fn flush(
    sink: &Arc<dyn ResultSink>,
    buffer: &mut VecDeque<CheckResult>,
    retry_attempts: &mut u32,
) {
    let count = buffer.len();
    histogram!(metric_names::STORAGE_BATCH_SIZE).record(count as f64);
    let start = Instant::now();
    // VecDeque may be non-contiguous; make_contiguous slides into one slice
    // (no realloc when capacity is enough) so the sink keeps its &[T] API.
    let slice = buffer.make_contiguous();
    match sink.write_batch(slice).await {
        Ok(()) => {
            counter!(metric_names::STORAGE_WRITES, "store" => "sink", "result" => "success")
                .increment(1);
            buffer.clear();
            *retry_attempts = 0;
        }
        Err(err) => {
            counter!(metric_names::STORAGE_WRITES, "store" => "sink", "result" => "failure")
                .increment(1);
            *retry_attempts = retry_attempts.saturating_add(1);
            if *retry_attempts >= MAX_FLUSH_RETRIES {
                tracing::error!(
                    ?err,
                    count,
                    retries = *retry_attempts,
                    "batcher flush failed; dropping batch after exhausted retries"
                );
                counter!(metric_names::STORAGE_DROPPED, "reason" => "retries_exhausted")
                    .increment(count as u64);
                buffer.clear();
                *retry_attempts = 0;
            } else {
                tracing::warn!(
                    ?err,
                    count,
                    attempt = *retry_attempts,
                    "batcher flush failed; retrying on next tick"
                );
            }
        }
    }
    histogram!(metric_names::STORAGE_WRITE_DURATION_MS).record(start.elapsed().as_millis() as f64);
}
