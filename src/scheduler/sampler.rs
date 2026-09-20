use std::sync::Arc;
use std::time::Duration;

use clickhouse::{Client as ChClient, Row};
use metrics::gauge;
use serde::Deserialize;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::domain::CheckResult;
use crate::metric_names;
use crate::scheduler::TargetRegistry;
use crate::worker::WorkerPool;

/// Postgres + ClickHouse telemetry sources. Present on the control plane,
/// absent on a probe-only agent (no database connection), so the agent never
/// registers the pool/parts gauges.
pub struct DbSources {
    pub pg_pool: PgPool,
    pub ch: ChClient,
}

pub fn spawn(
    pool: Arc<WorkerPool>,
    registry: Arc<TargetRegistry>,
    db: Option<DbSources>,
    result_tx: &mpsc::Sender<CheckResult>,
    sample_interval: Duration,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    // WeakSender so the sampler doesn't keep the result channel alive past
    // shutdown — the batcher's rx-drop fallback path needs the channel to
    // close once all real senders are gone.
    let queue_capacity = result_tx.max_capacity();
    let tx = result_tx.downgrade();
    tokio::spawn(run(
        pool,
        registry,
        db,
        tx,
        queue_capacity,
        sample_interval,
        shutdown,
    ))
}

async fn run(
    pool: Arc<WorkerPool>,
    registry: Arc<TargetRegistry>,
    db: Option<DbSources>,
    result_tx: mpsc::WeakSender<CheckResult>,
    queue_capacity: usize,
    sample_interval: Duration,
    shutdown: CancellationToken,
) {
    let mut ticker = interval(sample_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let g_in_flight = gauge!(metric_names::WORKERS_IN_FLIGHT);
    let g_breakers_open = gauge!(metric_names::BREAKERS_OPEN);
    let g_targets = gauge!(metric_names::TARGETS_TOTAL);
    let g_queue_depth = gauge!(metric_names::RESULT_QUEUE_DEPTH);
    let g_singleflight_slots = gauge!(metric_names::RDAP_SINGLEFLIGHT_SLOTS);
    let g_resident = gauge!(metric_names::PROCESS_RESIDENT_BYTES);

    // Register the DB gauges only when a database is wired, so the probe-only
    // agent's exposition stays free of pool/parts series it can't populate.
    let db = db.map(|src| DbGauges {
        pg_pool: src.pg_pool,
        ch: src.ch,
        size: gauge!(metric_names::PG_POOL_SIZE),
        idle: gauge!(metric_names::PG_POOL_IDLE),
        in_use: gauge!(metric_names::PG_POOL_IN_USE),
        parts: gauge!(metric_names::CLICKHOUSE_MAX_PART_COUNT),
    });

    let singleflight = pool.domain_expiry_runtime().singleflight.clone();

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ticker.tick() => {
                g_in_flight.set(pool.in_flight() as f64);
                g_breakers_open.set(pool.open_breakers() as f64);
                g_targets.set(registry.len() as f64);
                let depth = match result_tx.upgrade() {
                    Some(tx) => queue_capacity.saturating_sub(tx.capacity()),
                    None => 0,
                };
                g_queue_depth.set(depth as f64);
                g_singleflight_slots.set(singleflight.len() as f64);

                if let Some(bytes) = resident_bytes() {
                    g_resident.set(bytes as f64);
                }

                if let Some(db) = &db {
                    db.sample().await;
                }
            }
        }
    }
}

/// Pre-registered control-plane gauges plus the sources they read.
struct DbGauges {
    pg_pool: PgPool,
    ch: ChClient,
    size: metrics::Gauge,
    idle: metrics::Gauge,
    in_use: metrics::Gauge,
    parts: metrics::Gauge,
}

impl DbGauges {
    async fn sample(&self) {
        let pg_size = self.pg_pool.size() as usize;
        let pg_idle = self.pg_pool.num_idle();
        self.size.set(pg_size as f64);
        self.idle.set(pg_idle as f64);
        self.in_use.set(pg_size.saturating_sub(pg_idle) as f64);

        // Best-effort + time-bounded: the CH client has no read timeout, so a
        // hung (not refused) server could otherwise block this arm
        // indefinitely. The 2s cap bounds how long one hung read delays the
        // next tick (and thus shutdown); a failed/slow read skips it.
        match tokio::time::timeout(Duration::from_secs(2), max_part_count(&self.ch)).await {
            Ok(Some(parts)) => self.parts.set(parts),
            Ok(None) => {}
            Err(_) => tracing::debug!("clickhouse parts-count sample timed out"),
        }
    }
}

/// `MaxPartCountForPartition` from `system.asynchronous_metrics` (O(1),
/// precomputed by the server). `None` on any query error so a transient CH
/// outage doesn't take the gauge loop down with it.
async fn max_part_count(ch: &ChClient) -> Option<f64> {
    #[derive(Row, Deserialize)]
    struct V {
        value: f64,
    }
    match ch
        .query(
            "SELECT value FROM system.asynchronous_metrics \
             WHERE metric = 'MaxPartCountForPartition'",
        )
        .fetch_optional::<V>()
        .await
    {
        Ok(row) => row.map(|r| r.value),
        Err(err) => {
            tracing::debug!(?err, "clickhouse parts-count sample failed");
            None
        }
    }
}

/// Resident set size in bytes, sourced from `/proc/self/status` on Linux.
/// `None` on platforms without procfs — the gauge simply stays at its last
/// value (typically zero) and Grafana series for non-Linux dev runs are
/// absent rather than misleading.
#[cfg(target_os = "linux")]
fn resident_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    // VmRSS is reported in kB per kernel convention; multiply to bytes so
    // the metric is unit-clean for Grafana's bytes formatter.
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn resident_bytes() -> Option<u64> {
    None
}
