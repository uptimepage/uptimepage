use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use anyhow::Context;

use crate::domain::quota::{RetentionDays, evidence_ttl_days, raw_ttl_days};
use crate::error::Result;

/// Stamped on rows whose org isn't in the snapshot yet (just-created org, or a
/// boot before the first load). Matches the column DEFAULTs, so an unknown org
/// never over-retains.
const DEFAULT: RetentionDays = RetentionDays {
    row: 30,
    evidence: 7,
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Resolves an org's physical retention windows for the write path. Bulk-loaded
/// from `plans` so a flush reads a lock, never the DB; the 64-entry
/// request-path plan cache would thrash under per-flush, all-org reads.
#[derive(Clone, Default)]
pub struct OrgTtlDays {
    snapshot: Arc<RwLock<HashMap<Uuid, RetentionDays>>>,
}

impl OrgTtlDays {
    pub fn new() -> Self {
        Self::default()
    }

    /// Windows for each org under a single read lock — one acquisition per
    /// insert batch, not per row. Unknown orgs get [`DEFAULT`].
    pub fn days_for_each(&self, org_ids: impl IntoIterator<Item = Uuid>) -> Vec<RetentionDays> {
        let snap = self.snapshot.read().expect("org ttl snapshot poisoned");
        org_ids
            .into_iter()
            .map(|id| snap.get(&id).copied().unwrap_or(DEFAULT))
            .collect()
    }

    /// Windows for one org, for writers that insert a row at a time.
    pub fn days_for(&self, org_id: Uuid) -> RetentionDays {
        let snap = self.snapshot.read().expect("org ttl snapshot poisoned");
        snap.get(&org_id).copied().unwrap_or(DEFAULT)
    }

    /// Replace the snapshot from the shared retention reader, so physical TTL
    /// and the read-side window resolve the plan through the same path.
    pub async fn refresh(&self, pool: &PgPool) -> Result<usize> {
        let next = retention_days_by_org(pool).await?;
        let n = next.len();
        *self.snapshot.write().expect("org ttl snapshot poisoned") = next;
        Ok(n)
    }
}

/// Refreshes `ttl` every [`REFRESH_INTERVAL`] until cancelled. The first tick
/// fires immediately, so the snapshot warms right after spawn — no blocking
/// load on the boot path. A failed refresh keeps serving the last snapshot.
pub fn spawn_refresh(ttl: OrgTtlDays, pool: PgPool, shutdown: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = ticker.tick() => {
                    if let Err(err) = ttl.refresh(&pool).await {
                        tracing::warn!(?err, "org ttl refresh failed; serving last snapshot");
                    }
                }
            }
        }
    })
}

/// Bulk `org_id → physical retention days`, one query, the same ceilings as
/// [`crate::domain::Plan::raw_window_days`] via [`raw_ttl_days`] / [`evidence_ttl_days`]. Feeds
/// the write-path TTL snapshot ([`OrgTtlDays`]), which must read
/// every active org without thrashing the plan cache. Plan-level:
/// neither column carries an override or add-on today, so no override
/// folding is applied.
pub async fn retention_days_by_org(pool: &PgPool) -> Result<HashMap<Uuid, RetentionDays>> {
    let rows: Vec<(Uuid, i32, i32)> = sqlx::query_as(
        "SELECT /* SAFE: every org's physical retention window, read once per refresh for the write-path TTL snapshot; returns plan numbers, no tenant data */ \
         o.id, p.raw_days, p.evidence_days \
         FROM organizations o \
         JOIN accounts a ON a.id = o.account_id \
         JOIN plans p ON p.id = a.plan_id",
    )
    .fetch_all(pool)
    .await
    .context("load retention days by org")?;
    Ok(rows
        .into_iter()
        .map(|(id, raw, evidence)| {
            (
                id,
                RetentionDays {
                    row: raw_ttl_days(raw),
                    evidence: evidence_ttl_days(evidence, raw),
                },
            )
        })
        .collect())
}
