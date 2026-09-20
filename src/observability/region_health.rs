//! Per-region probe quality from ClickHouse: throughput, up-count, and p95
//! latency over a recent window, grouped by region. A control-plane task — the
//! brain already ingests every agent's results, so this needs no agent scraping
//! and its cost tracks regions, not customers. Only regions producing results
//! in the window appear; a region gone fully dark drops out and is detected by
//! `uptimepage_region_agents_up` (agent_health), not here.

use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use clickhouse::{Client as ChClient, Row};
use serde::Deserialize;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::metric_names;

const TICK: Duration = Duration::from_secs(30);
const WINDOW_SECS: u32 = 300;
// The CH client has no read timeout; bound the read so a hung (not refused)
// server can't stall the gauge loop until the next tick.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// One region's health over the recent window.
#[derive(Row, Deserialize, Debug, Clone, PartialEq)]
pub struct RegionStat {
    pub region: String,
    pub total: u64,
    pub up: u64,
    pub p95_ms: f64,
}

pub async fn run(ch: ChClient, shutdown: CancellationToken) {
    let mut ticker = interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ticker.tick() => {
                if let Err(err) = sweep(&ch).await {
                    tracing::warn!(?err, "region health gauge sweep failed; serving last values");
                }
            }
        }
    }
}

async fn sweep(ch: &ChClient) -> Result<()> {
    let stats = match tokio::time::timeout(QUERY_TIMEOUT, collect(ch, WINDOW_SECS)).await {
        Ok(res) => res?,
        Err(_) => {
            tracing::warn!("region health sweep timed out; serving last values");
            return Ok(());
        }
    };
    for s in stats {
        metrics::gauge!(metric_names::REGION_CHECKS_WINDOW, "region" => s.region.clone())
            .set(s.total as f64);
        metrics::gauge!(metric_names::REGION_CHECKS_UP_WINDOW, "region" => s.region.clone())
            .set(s.up as f64);
        metrics::gauge!(metric_names::REGION_CHECK_LATENCY_P95_MS, "region" => s.region)
            .set(s.p95_ms);
    }
    Ok(())
}

/// One grouped read over the last `window_secs`, merged from the per-minute
/// rollup so the scan is O(minutes) not O(raw checks). Operator-wide across
/// every org by design, like the inventory gauges. The cutoff is computed
/// app-side and bound as a unix timestamp (matching the other range reads) so
/// it does not depend on ClickHouse interval parsing. p95 is the approximate
/// quantile merged from the rollup state, fine for a health signal.
pub async fn collect(ch: &ChClient, window_secs: u32) -> Result<Vec<RegionStat>> {
    let cutoff = Utc::now().timestamp() - i64::from(window_secs);
    let rows = ch
        .query(
            "SELECT region, \
                    countMerge(total_checks) AS total, \
                    countIfMerge(up_checks) AS up, \
                    quantilesMerge(0.5, 0.95)(duration_quantiles)[2] AS p95_ms \
             FROM check_results_1m \
             WHERE minute >= toStartOfMinute(fromUnixTimestamp(?)) \
             GROUP BY region",
        )
        .bind(cutoff)
        .fetch_all::<RegionStat>()
        .await
        .context("clickhouse per-region health sweep")?;
    Ok(rows)
}
