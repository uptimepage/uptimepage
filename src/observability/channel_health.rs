//! Standing count of notification channels that have stopped delivering.
//!
//! The dispatch counters only move when an incident pages, so a dead endpoint
//! bound to quiet monitors emits nothing at all.

use std::time::Duration;

use anyhow::Context;
use sqlx::PgPool;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::domain::ChannelKind;
use crate::error::Result;
use crate::metric_names;

const TICK: Duration = Duration::from_secs(60);

pub async fn run(pg: PgPool, failure_limit: u32, shutdown: CancellationToken) {
    let mut ticker = interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ticker.tick() => {
                if let Err(err) = sweep(&pg, failure_limit).await {
                    tracing::warn!(?err, "channel health gauge sweep failed; serving last values");
                }
            }
        }
    }
}

/// Must stay in step with `NotificationChannel::is_failing`, which is what the
/// console shows and the owner mail fires on. A deleted org's channels are
/// excluded: monitoring is paused there, so the run is frozen at whatever it
/// was and no operator can act on it.
pub async fn failing_by_transport(pg: &PgPool, failure_limit: u32) -> Result<Vec<(String, i64)>> {
    if failure_limit == 0 {
        return Ok(Vec::new());
    }
    sqlx::query_as(
        "SELECT c.kind, count(*) FROM notification_channels c \
         JOIN organizations o ON o.id = c.org_id AND o.deleted_at IS NULL \
         WHERE c.enabled AND c.consecutive_failures >= $1 \
           AND NOT (c.kind = 'email' AND c.verified_at IS NULL) \
         GROUP BY c.kind \
         /* SAFE: operator-wide delivery-health gauge, counts every live org by design */",
    )
    .bind(i32::try_from(failure_limit).unwrap_or(i32::MAX))
    .fetch_all(pg)
    .await
    .context("count failing notification channels by kind")
    .map_err(Into::into)
}

async fn sweep(pg: &PgPool, failure_limit: u32) -> Result<()> {
    let by_kind = failing_by_transport(pg, failure_limit).await?;
    // Emit every transport so one that recovers reports 0, not its last value.
    for kind in ChannelKind::ALL {
        let transport = kind.as_db_str();
        let n = by_kind
            .iter()
            .find(|(k, _)| k == transport)
            .map_or(0, |(_, n)| *n);
        metrics::gauge!(metric_names::CHANNELS_FAILING, "transport" => transport).set(n as f64);
    }
    Ok(())
}
