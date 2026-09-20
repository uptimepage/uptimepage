//! Configured-monitor, active-user and notification-channel counts from
//! Postgres, refreshed on a slow cadence and scrape-cached so request load
//! never reaches Postgres.

use std::time::Duration;

use anyhow::Context;
use sqlx::PgPool;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::domain::{ChannelKind, CheckSpec};
use crate::error::Result;
use crate::metric_names;

const TICK: Duration = Duration::from_secs(60);

pub async fn run(pg: PgPool, shutdown: CancellationToken) {
    let mut ticker = interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ticker.tick() => {
                if let Err(err) = sweep(&pg).await {
                    tracing::warn!(?err, "inventory gauge sweep failed; serving last values");
                }
            }
        }
    }
}

async fn sweep(pg: &PgPool) -> Result<()> {
    let by_kind: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kind, count(*) FROM targets WHERE enabled GROUP BY kind \
             /* SAFE: operator-wide inventory gauge, counts every org by design */",
    )
    .fetch_all(pg)
    .await
    .context("count enabled targets by kind")?;
    // Emit every kind so one that drains to zero reports 0, not its last value.
    for kind in CheckSpec::ALL_KINDS {
        let n = by_kind
            .iter()
            .find(|(k, _)| k == kind)
            .map_or(0, |(_, n)| *n);
        metrics::gauge!(metric_names::TARGETS_ENABLED, "kind" => kind).set(n as f64);
    }

    let active_users: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE deleted_at IS NULL")
            .fetch_one(pg)
            .await
            .context("count active users")?;
    metrics::gauge!(metric_names::USERS_ACTIVE).set(active_users as f64);

    let channels: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT c.kind, count(*), count(DISTINCT c.org_id) FROM notification_channels c \
             JOIN organizations o ON o.id = c.org_id AND o.deleted_at IS NULL \
             WHERE c.enabled GROUP BY c.kind \
             /* SAFE: operator-wide inventory gauge, counts every live org by design */",
    )
    .fetch_all(pg)
    .await
    .context("count enabled notification channels by kind")?;
    // Emit every kind so one nobody uses reads 0 rather than going absent.
    for kind in ChannelKind::ALL {
        let db_str = kind.as_db_str();
        let row = channels.iter().find(|(k, _, _)| k == db_str);
        metrics::gauge!(metric_names::NOTIFICATION_CHANNELS, "kind" => db_str)
            .set(row.map_or(0, |(_, n, _)| *n) as f64);
        metrics::gauge!(metric_names::NOTIFICATION_CHANNEL_ORGS, "kind" => db_str)
            .set(row.map_or(0, |(_, _, orgs)| *orgs) as f64);
    }

    // Summing the per-kind org counts double-counts an org using two transports.
    let orgs_with_channels: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT c.org_id) FROM notification_channels c \
             JOIN organizations o ON o.id = c.org_id AND o.deleted_at IS NULL \
             WHERE c.enabled \
             /* SAFE: operator-wide inventory gauge, counts every live org by design */",
    )
    .fetch_one(pg)
    .await
    .context("count orgs with a notification channel")?;
    metrics::gauge!(metric_names::ORGS_WITH_CHANNELS).set(orgs_with_channels as f64);

    Ok(())
}
